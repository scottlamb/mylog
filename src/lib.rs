//! A simple stderr-based logger which supports a couple formats and asynchronous operation.

extern crate chrono;
extern crate libc;
extern crate log;
extern crate parking_lot;

mod spec;

use log::{Record, Level, Metadata};
use parking_lot::{Condvar, Mutex};
use spec::Specification;
use std::io::{self, Write};
use std::mem;
use std::sync::Arc;
use std::thread;

const MAX_ENTRY_SIZE: usize = 1<<16;
const BUF_SIZE: usize = 1<<20;

/// The format of logged messages.
#[derive(Debug)]
pub enum Format {
    /// Log format modelled after the Google [glog](https://github.com/google/glog) library.
    /// Typical entry:
    /// ```
    /// I0308 213124.255 main moonfire_nvr] Success.
    /// Lmmdd HHMMSS.FFF TTTT PPPPPPPPPPPP] ...
    /// L    = level:
    ///        E = error!
    ///        W = warn!
    ///        I = info!
    ///        D = debug!
    ///        T = trace!
    /// mm   = month
    /// dd   = day
    /// HH   = hour (using a 24-hour clock)
    /// MM   = minute
    /// SS   = section
    /// FFF  = fractional portion of the second
    /// TTTT = thread name (if set) or tid (otherwise)
    /// PPPP = module path
    /// ...  = the message supplied to the log macro.
    /// ```
    Google,

    /// Google log format, adapted for systemd output. See
    /// [sd-daemon(3)](https://www.freedesktop.org/software/systemd/man/sd-daemon.html).
    /// The date and time are omitted; the prefix is replaced with one understood by systemd.
    /// Typical entry:
    /// ```
    /// <5>main moonfire_nvr] Success.
    /// ```
    ///
    /// The supported log levels are as follows:
    /// ```
    /// <3> = SD_ERR     = error!
    /// <4> = SD_WARNING = warn!
    /// <5> = SD_NOTICE  = info!
    /// <6> = SD_INFO    = debug!
    /// <7> = SD_DEBUG   = trace!
    /// ```
    GoogleSystemd,
}

impl Format {
    fn write(&self, record: &Record, c: &mut io::Cursor<&mut [u8]>) -> Result<(), io::Error> {
        match *self {
            Format::Google => Format::write_google(record, c),
            Format::GoogleSystemd => Format::write_google_systemd(record, c),
        }
    }

    fn write_google(record: &Record, c: &mut io::Cursor<&mut [u8]>) -> Result<(), io::Error> {
        let level = match record.level() {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
            Level::Trace => "T",
        };
        const TIME_FORMAT: &'static str = "%m%d %H%M%S%.3f";
        let p = record.module_path().unwrap_or("");
        if let Some(name) = thread::current().name() {
            write!(c, "{}{} {} {}] {}", level, chrono::Local::now().format(TIME_FORMAT), name,
                   p, record.args())
        } else {
            write!(c, "{}{} {} {}] {}", level, chrono::Local::now().format(TIME_FORMAT),
                   unsafe { libc::getpid() }, p, record.args())
        }
    }

    fn write_google_systemd(record: &Record, c: &mut io::Cursor<&mut [u8]>)
                          -> Result<(), io::Error> {
        let level = match record.level() {
            Level::Error => "<3>",  // SD_ERR
            Level::Warn  => "<4>",  // SD_WARNING
            Level::Info  => "<5>",  // SD_NOTICE
            Level::Debug => "<6>",  // SD_INFO
            Level::Trace => "<7>",  // SD_DEBUG
        };
        let p = record.module_path().unwrap_or("");
        if let Some(name) = thread::current().name() {
            write!(c, "{}{} {}] {}", level, name, p, record.args())
        } else {
            write!(c, "{}{} {}] {}", level, unsafe { libc::getpid() }, p, record.args())
        }
    }
}

#[derive(Debug)]
pub enum Destination {
    Stdout,
    Stderr,
}

pub struct Builder {
    spec: Option<Specification>,
    fmt: Format,
    dest: Destination,
}

impl Builder {
    pub fn new() -> Self {
        Builder{
            spec: None,
            fmt: Format::Google,
            dest: Destination::Stderr,
        }
    }

    pub fn set_format(mut self, fmt: Format) -> Self {
        self.fmt = fmt;
        self
    }

    pub fn set_spec(mut self, spec: &str) -> Self {
        self.spec = Some(Specification::new(spec));
        self
    }

    /// Sets the log destination; default is stderr.
    pub fn set_destination(mut self, dest: Destination) -> Self {
        self.dest = dest;
        self
    }

    pub fn build(self) -> Handle {
        Handle(Arc::new(Logger{
            inner: Mutex::new(LoggerInner {
                buf: Vec::with_capacity(BUF_SIZE),
                use_async: false,
            }),
            wake_consumer: Condvar::new(),
            wake_producers: Condvar::new(),
            spec: self.spec.unwrap_or_else(|| Specification::new("")),
            fmt: self.fmt,
            dest: self.dest,
        }))
    }
}

/// A handle to a logger which can be used to install it globally and/or enable asynchronous
/// logging.
#[derive(Clone)]
pub struct Handle(Arc<Logger>);

impl Handle {
    /// Installs this logger as the global logger used by the `log` crate.
    /// Can only be called once in the lifetime of the program.
    pub fn install(self) -> Result<(), log::SetLoggerError> {
        let logger = self.0;

        // Leak an instance of the Arc, so that the pointer lives forever.
        // This allows transmuting it to 'static soundly.
        let l = unsafe { ::std::mem::transmute::<&log::Log, &'static log::Log>(&*logger) };
        log::set_logger(l)?;
        log::set_max_level(logger.spec.max);
        mem::forget(logger);
        Ok(())
    }

    /// Enables asynchronous logging until the returned `AsyncHandle` is dropped.
    /// Typically this is called during `main` and held until shortly before returning to the OS.
    /// During asynchronous mode, logging calls will not block for I/O until at least 1 MiB has
    /// been buffered.
    pub fn async<'a>(&'a mut self) -> AsyncHandle<'a> {
        let was_async = {
            let mut l = self.0.inner.lock();
            mem::replace(&mut l.use_async, true)
        };
        assert!(!was_async);
        let logger = self.0.clone();
        AsyncHandle{
            logger: self,
            join: Some(thread::Builder::new().name("logger".to_owned())
                                             .spawn(move || logger.run_async())
                                             .unwrap()),
        }
    }
}

pub struct AsyncHandle<'a> {
    logger: &'a mut Handle,
    join: Option<thread::JoinHandle<()>>,
}

impl<'a> Drop for AsyncHandle<'a> {
    fn drop(&mut self) {
        let was_async = {
            let mut l = self.logger.0.inner.lock();
            self.logger.0.wake_consumer.notify_one();
            mem::replace(&mut l.use_async, false)
        };
        assert!(was_async);
        self.join.take().unwrap().join().unwrap();
    }
}

struct Logger {
    inner: Mutex<LoggerInner>,
    wake_consumer: Condvar,
    wake_producers: Condvar,
    fmt: Format,
    spec: Specification,
    dest: Destination,
}

struct LoggerInner {
    buf: Vec<u8>,
    use_async: bool,
}

impl Logger {
    fn write_all(&self, buf: &[u8]) -> Result<(), io::Error> {
        match self.dest {
            Destination::Stderr => io::stderr().write_all(&buf),
            Destination::Stdout => io::stdout().write_all(&buf),
        }
    }

    fn run_async(&self) {
        let mut buf = Vec::with_capacity(BUF_SIZE);
        let mut use_async = true;
        while use_async {
            // Move data to buf.
            {
                let mut l = self.inner.lock();
                if l.buf.is_empty() && l.use_async {
                    self.wake_consumer.wait(&mut l);
                }
                use_async = l.use_async;
                buf.clear();
                mem::swap(&mut buf, &mut l.buf);
                self.wake_producers.notify_all();
            };

            // Write buf.
            if !buf.is_empty() {
                let _ = self.write_all(&buf);
            }
        }
    }
}

impl log::Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.spec.get_level(metadata.target()) >= metadata.level()
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) { return; }
        let mut buf: [u8; MAX_ENTRY_SIZE] = unsafe { mem::uninitialized() };
        let len = {
            let mut c = io::Cursor::new(&mut buf[.. MAX_ENTRY_SIZE-1]);
            match self.fmt.write(record, &mut c) {
                Err(ref e) if e.kind() == io::ErrorKind::WriteZero => {},  // truncated. okay.
                Err(_) => return,  // unable to write log entry. skip.
                Ok(()) => {},
            }
            c.position() as usize
        };
        buf[len] = b'\n';  // always terminate with a newline (even if truncated).
        let msg = &buf[0 .. len+1];
        let mut l = self.inner.lock();

        if !l.use_async {
            let _ = self.write_all(msg);
            return;
        }

        // Wait for there to be room in the buffer, then copy and notify the logger thread.
        while l.buf.len() + msg.len() > BUF_SIZE {
            self.wake_producers.wait(&mut l);
        }
        l.buf.extend_from_slice(msg);
        self.wake_consumer.notify_one();
    }

    fn flush(&self) {
        let mut l = self.inner.lock();
        if l.use_async {
            while !l.buf.is_empty() {
                self.wake_producers.wait(&mut l);
            }
        }
    }
}
