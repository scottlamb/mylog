use log::LevelFilter;
use std::io::{self, Write};

/// A logging specification: controls which modules log at what level.
pub struct Specification {
    /// A list of (filter, prefix), with the most specific prefixes first.
    directives: Vec<(LevelFilter, String)>,

    /// The most detailed log level of any module.
    pub max: LevelFilter,
}

impl Specification {
    pub fn new(spec: &str) -> Self {
        let mut v: Vec<(LevelFilter, String)> = Vec::new();
        for d in spec.split(',') {
            if d.is_empty() { continue; }
            let mut parts = d.splitn(2, '=');
            let (level, prefix) = match (parts.next(), parts.next()) {
                (Some(p), None) => match p.parse() {
                    Ok(level) => (level, String::new()),
                    Err(_) => (LevelFilter::max(), p.to_owned()),
                },
                (Some(p), Some(l)) => match l.parse() {
                    Ok(l) => (l, p.to_owned()),
                    Err(_) => {
                        let _ = writeln!(io::stderr(),
                                         "logging directive {:?} has unparseable log level", d);
                        continue;
                    },
                },
                (None, _) => unreachable!(),
            };
            v.push((level, prefix));
        }

        if v.is_empty() {
            v.push((LevelFilter::Error, String::new()));
        }

        // Sort the prefixes: longest to shortest.
        v.sort_by_key(|&(_, ref p)| usize::max_value() - p.len());
        let max = v.iter().map(|&(level, _)| level).max().unwrap_or(LevelFilter::Off);
        Specification {
            directives: v,
            max: max,
        }
    }

    pub fn get_level(&self, module: &str) -> LevelFilter {
        for &(level, ref prefix) in &self.directives {
            if module.starts_with(prefix) {
                return level;
            }
        }
        LevelFilter::Off
    }
}

#[cfg(test)]
mod tests {
    use log::LevelFilter;
    use super::Specification;

    #[test]
    fn default() {
        let spec = Specification::new("");
        assert_eq!(spec.get_level("foo"), LevelFilter::Error);
        assert_eq!(spec.max, LevelFilter::Error);
    }

    #[test]
    fn blah() {
        let spec = Specification::new("info,crate1=off,crate2=warn,crate2::inner=trace,crate3");
        assert_eq!(spec.max, LevelFilter::Trace);
        assert_eq!(spec.get_level("crate1"), LevelFilter::Off);
        assert_eq!(spec.get_level("crate2::something"), LevelFilter::Warn);
        assert_eq!(spec.get_level("crate2::inner::something"), LevelFilter::Trace);
        assert_eq!(spec.get_level("crate3"), LevelFilter::Trace);
        assert_eq!(spec.get_level("crate4"), LevelFilter::Info);
    }
}
