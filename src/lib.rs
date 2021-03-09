//! A simple stderr-based logger which supports a couple formats and asynchronous operation.

mod entry_buf;
mod spec;

use crate::entry_buf::EntryBuf;
use log::{Level, Metadata, Record};
use parking_lot::{Condvar, Mutex};
use spec::Specification;
use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::Arc;
use std::thread;

/// The maximum number of bytes of a single log entry including the trailing `\n`.
///
/// Must be at least one (to fit the trailing `\n`) and must fit within the program stack.
/// Thus it can be safely assumed it is less than `isize::max()` as well.
///
/// If a log call tries to write more than this size, the entry will be truncated. Truncated
/// entries may have an invalid UTF-8 sequence but will always end in `\n`.
const MAX_ENTRY_SIZE: usize = 1 << 16;

/// The size of the (heap-allocated) asynchronous buffer.
///
/// Twice this size will be allocated in total due to a double-buffering scheme.
///
/// Entries are copied to this buffer atomically, so this must be at least `MAX_ENTRY_SIZE` or
/// `Logger::log` could block forever waiting for space.
const ASYNC_BUF_SIZE: usize = 1 << 20;

/// The format of logged messages.
#[derive(Debug)]
pub enum Format {
    /// Log format modelled after the Google [glog](https://github.com/google/glog) library.
    /// Typical entry:
    /// ```text
    /// I20210308 21:31:24.255 main moonfire_nvr] Success.
    /// LYYYYmmdd HH:MM:SS.FFF TTTT PPPPPPPPPPPP] ...
    /// L    = level:
    ///        E = error!
    ///        W = warn!
    ///        I = info!
    ///        D = debug!
    ///        T = trace!
    /// YYYY = year
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
    /// ```text
    /// <5>main moonfire_nvr] Success.
    /// ```
    ///
    /// The supported log levels are as follows:
    /// ```text
    /// <3> = SD_ERR     = error!
    /// <4> = SD_WARNING = warn!
    /// <5> = SD_NOTICE  = info!
    /// <6> = SD_INFO    = debug!
    /// <7> = SD_DEBUG   = trace!
    /// ```
    GoogleSystemd,
}

impl std::str::FromStr for Format {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "google" => Ok(Format::Google),
            "google-systemd" => Ok(Format::GoogleSystemd),
            _ => Err(()),
        }
    }
}

impl Format {
    fn write(
        &self,
        record: &Record,
        buf: &mut EntryBuf<entry_buf::Writing>,
    ) -> Result<(), std::fmt::Error> {
        match *self {
            Format::Google => Format::write_google(record, buf),
            Format::GoogleSystemd => Format::write_google_systemd(record, buf),
        }
    }

    fn write_google(
        record: &Record,
        buf: &mut EntryBuf<entry_buf::Writing>,
    ) -> Result<(), std::fmt::Error> {
        let level = match record.level() {
            Level::Error => "E",
            Level::Warn => "W",
            Level::Info => "I",
            Level::Debug => "D",
            Level::Trace => "T",
        };
        const TIME_FORMAT: &str = "%Y%m%d %H:%M:%S%.3f";
        let p = record.module_path().unwrap_or("");
        let t = thread::current();
        if let Some(name) = t.name() {
            write!(
                buf,
                "{}{} {} {}] {}",
                level,
                chrono::Local::now().format(TIME_FORMAT),
                name,
                p,
                record.args()
            )
        } else {
            write!(
                buf,
                "{}{} {:?} {}] {}",
                level,
                chrono::Local::now().format(TIME_FORMAT),
                t.id(),
                p,
                record.args()
            )
        }
    }

    fn write_google_systemd(
        record: &Record,
        buf: &mut EntryBuf<entry_buf::Writing>,
    ) -> Result<(), std::fmt::Error> {
        let level = match record.level() {
            Level::Error => "<3>", // SD_ERR
            Level::Warn => "<4>",  // SD_WARNING
            Level::Info => "<5>",  // SD_NOTICE
            Level::Debug => "<6>", // SD_INFO
            Level::Trace => "<7>", // SD_DEBUG
        };
        let p = record.module_path().unwrap_or("");
        let t = thread::current();
        if let Some(name) = t.name() {
            write!(buf, "{}{} {}] {}", level, name, p, record.args())
        } else {
            write!(buf, "{}{:?} {}] {}", level, t.id(), p, record.args())
        }
    }
}

#[derive(Debug)]
pub enum Destination {
    Stderr,
    Stdout,
}

pub struct Builder {
    spec: Option<Specification>,
    fmt: Format,
    dest: Destination,
}

impl Builder {
    pub fn new() -> Self {
        Builder {
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
        Handle(Arc::new(Logger {
            inner: Mutex::new(LoggerInner {
                async_buf: Vec::with_capacity(ASYNC_BUF_SIZE),
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

impl Default for Builder {
    fn default() -> Self {
        Self::new()
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
        let l: &'static Logger = unsafe { &*Arc::into_raw(logger) };
        log::set_logger(l)?;
        log::set_max_level(l.spec.max);
        Ok(())
    }

    /// Enables asynchronous logging until the returned `AsyncHandle` is dropped.
    /// Typically this is called during `main` and held until shortly before returning to the OS.
    /// During asynchronous mode, logging calls will not block for I/O until at least 1 MiB has
    /// been buffered.
    pub fn async_scope(&mut self) -> AsyncHandle {
        let was_async = {
            let mut l = self.0.inner.lock();
            std::mem::replace(&mut l.use_async, true)
        };
        assert!(!was_async);
        let logger = self.0.clone();
        AsyncHandle {
            logger: self,
            join: Some(
                thread::Builder::new()
                    .name("logger".to_owned())
                    .spawn(move || logger.run_async())
                    .unwrap(),
            ),
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
            std::mem::replace(&mut l.use_async, false)
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
    async_buf: Vec<u8>,
    use_async: bool,
}

impl Logger {
    /// Writes from `buf` to the target (stdout or stderr).
    ///
    /// When operating asynchronously, called only from `run_async`.
    /// When operating synchronously, called directly from `log`.
    fn write_all(&self, buf: &[u8]) -> Result<(), std::io::Error> {
        match self.dest {
            Destination::Stderr => std::io::stderr().write_all(&buf),
            Destination::Stdout => std::io::stdout().write_all(&buf),
        }
    }

    fn run_async(&self) {
        let mut buf = Vec::with_capacity(ASYNC_BUF_SIZE);
        let mut use_async = true;
        while use_async {
            // Swap logger's async_buf (which has bytes to write) with an empty buf.
            {
                let mut l = self.inner.lock();
                if l.async_buf.is_empty() && l.use_async {
                    self.wake_consumer.wait(&mut l);
                }
                use_async = l.use_async;
                buf.clear();
                std::mem::swap(&mut buf, &mut l.async_buf);
                self.wake_producers.notify_all();
            };

            // Write buf.
            if !buf.is_empty() {
                // This can throw an error, but what are going to do, log it? Discard.
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
        if !self.enabled(record.metadata()) {
            return;
        }

        // Always write into an EntryBuf first. This minimizes thread contention, whether async is
        // enabled or not.
        let mut buf = EntryBuf::new();

        // Write as much as fits; ignore truncation, which is the only possible error.
        let _ = self.fmt.write(record, &mut buf);
        let buf = buf.terminate();
        let buf = buf.get();

        let mut l = self.inner.lock();

        if !l.use_async {
            let _ = self.write_all(buf);
            return;
        }

        // Wait for there to be room in the buffer, then copy and notify the logger thread.
        // Theoretically a large entry could be starved by shorter entries, but it seems unlikely
        // to be problematic.
        while l.async_buf.len() + buf.len() > ASYNC_BUF_SIZE {
            self.wake_producers.wait(&mut l);
        }
        l.async_buf.extend_from_slice(buf);
        self.wake_consumer.notify_one();
    }

    fn flush(&self) {
        let mut l = self.inner.lock();
        if l.use_async {
            while !l.async_buf.is_empty() {
                self.wake_producers.wait(&mut l);
            }
        }
    }
}
