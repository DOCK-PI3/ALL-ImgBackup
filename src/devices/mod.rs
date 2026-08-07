use std::io;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub path: String,
    pub name: String,
    pub vendor: String,
    pub serial: String,
    pub bus: String,
    pub capacity: u64,
    pub removable: bool,
    pub system: bool,
    pub mounted: bool,
    pub mount_points: Vec<String>,
}

impl DeviceInfo {
    pub fn subtitle(&self) -> String {
        let mut parts = vec![format_bytes(self.capacity), self.bus.clone()];
        if self.removable {
            parts.push("extraíble".into());
        }
        if self.mounted {
            parts.push(format!("montado en {}", self.mount_points.join(", ")));
        }
        parts.join(" · ")
    }
}

pub fn scan() -> io::Result<Vec<DeviceInfo>> {
    platform::scan()
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes == 0 {
        return "0 B".into();
    }
    let index = ((bytes as f64).log(1000.0).floor() as usize).min(UNITS.len() - 1);
    let value = bytes as f64 / 1000f64.powi(index as i32);
    if index == 0 || value >= 100.0 {
        format!("{value:.0} {}", UNITS[index])
    } else {
        format!("{value:.1} {}", UNITS[index])
    }
}

#[cfg(windows)]
mod platform;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod platform;

#[cfg(not(any(windows, target_os = "linux")))]
mod platform {
    use super::DeviceInfo;
    use std::io;

    pub fn scan() -> io::Result<Vec<DeviceInfo>> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ALL ImgBackup sólo admite Windows y Linux",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::format_bytes;

    #[test]
    fn formats_capacities() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(1_000_000), "1.0 MB");
        assert_eq!(format_bytes(64_000_000_000), "64.0 GB");
    }
}
