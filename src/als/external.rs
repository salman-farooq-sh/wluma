use super::Scale;
use anyhow::{anyhow, Context, Result};
use smol::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use smol::lock::Mutex;
use smol::net::unix::UnixStream;
use smol::Timer;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_millis(100);
const READ_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_LINE_LENGTH: u64 = 64;

pub struct Als {
    path: PathBuf,
    scale: Scale,
    reader: Mutex<Option<BufReader<UnixStream>>>,
}

impl Als {
    pub fn new(path: impl AsRef<Path>, scale: Scale) -> Self {
        Self {
            path: path.as_ref().to_path_buf(),
            scale,
            reader: Mutex::new(None),
        }
    }

    pub async fn get(&self) -> Result<u64> {
        let mut reader = self.reader.lock().await;
        if reader.is_none() {
            let stream = UnixStream::connect(&self.path)
                .await
                .with_context(|| format!("Unable to connect to '{}'", self.path.display()))?;
            log::debug!("Connected to external ALS at '{}'", self.path.display());
            *reader = Some(BufReader::new(stream));
        }

        let mut line = String::new();
        let result = smol::future::race(
            reader
                .as_mut()
                .expect("External ALS connection must be open")
                .take(MAX_LINE_LENGTH)
                .read_line(&mut line),
            async {
                Timer::after(READ_TIMEOUT).await;
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "External ALS timed out",
                ))
            },
        )
        .await;
        match result {
            Ok(0) => {
                *reader = None;
                Err(anyhow!("External ALS connection closed"))
            }
            Ok(_) if !line.ends_with('\n') => {
                *reader = None;
                Err(anyhow!("External ALS value is not newline-terminated"))
            }
            Ok(_) => self.parse(&line),
            Err(error) => {
                *reader = None;
                Err(error).context("Unable to read external ALS value")
            }
        }
    }

    pub fn poll_interval(&self) -> Duration {
        POLL_INTERVAL
    }

    pub fn scale(&self) -> Scale {
        self.scale
    }

    fn parse(&self, line: &str) -> Result<u64> {
        let value = line
            .trim()
            .parse::<f64>()
            .context("External ALS value is not a number")?;
        if !value.is_finite() || value < 0.0 || value > u64::MAX as f64 {
            return Err(anyhow!("External ALS value must be a non-negative number"));
        }
        if self.scale == Scale::Linear && value > 100.0 {
            return Err(anyhow!(
                "Linear external ALS value must be between 0 and 100"
            ));
        }
        let value = value.round() as u64;
        log::trace!("ALS (external): {value}");
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use smol::io::AsyncWriteExt;
    use smol::net::unix::UnixListener;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reads_a_line_from_the_socket() {
        smol::block_on(async {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("wluma-{unique}.sock"));
            let listener = UnixListener::bind(&path).unwrap();
            let server = smol::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"42.5\n").await.unwrap();
            });

            let als = Als::new(&path, Scale::Lux);
            assert_eq!(43, als.get().await.unwrap());
            server.await;
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn validates_source_domain() {
        let lux = Als::new("unused", Scale::Lux);
        assert_eq!(42, lux.parse("42.4\n").unwrap());
        assert_eq!(1000, lux.parse("1000").unwrap());

        let linear = Als::new("unused", Scale::Linear);
        assert_eq!(43, linear.parse("42.5\n").unwrap());
        assert!(linear.parse("101").is_err());
        assert!(linear.parse("-1").is_err());
        assert!(linear.parse("invalid").is_err());
    }

    #[test]
    fn rejects_incomplete_lines() {
        smol::block_on(async {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("wluma-{unique}.sock"));
            let listener = UnixListener::bind(&path).unwrap();
            let server = smol::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"42").await.unwrap();
            });

            let als = Als::new(&path, Scale::Lux);
            assert!(als.get().await.is_err());
            server.await;
            std::fs::remove_file(path).unwrap();
        });
    }

    #[test]
    fn reconnects_after_the_connection_closes() {
        smol::block_on(async {
            let unique = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("wluma-{unique}.sock"));
            let listener = UnixListener::bind(&path).unwrap();
            let server = smol::spawn(async move {
                let _ = listener.accept().await.unwrap();
                let (mut stream, _) = listener.accept().await.unwrap();
                stream.write_all(b"42\n").await.unwrap();
            });

            let als = Als::new(&path, Scale::Lux);
            assert!(als.get().await.is_err());
            assert_eq!(42, als.get().await.unwrap());
            server.await;
            std::fs::remove_file(path).unwrap();
        });
    }
}
