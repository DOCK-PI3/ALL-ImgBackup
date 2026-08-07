use super::DeviceInfo;
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    path::Path,
};

pub fn scan() -> io::Result<Vec<DeviceInfo>> {
    let mounts = read_mounts();
    let system_parents = system_disk_names(&mounts);
    let mut devices = Vec::new();

    for entry in fs::read_dir("/sys/class/block")? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let sys_path = entry.path();

        if is_virtual_or_partition(&name, &sys_path) {
            continue;
        }

        let capacity = read_trimmed(sys_path.join("size"))
            .parse::<u64>()
            .unwrap_or_default()
            .saturating_mul(512);
        if capacity == 0 {
            continue;
        }

        let removable = read_trimmed(sys_path.join("removable")) == "1";
        let model = read_trimmed(sys_path.join("device/model"));
        let vendor = read_trimmed(sys_path.join("device/vendor"));
        let serial = read_trimmed(sys_path.join("device/serial"));
        let bus = detect_bus(&sys_path);
        let related_mounts = mounts_for_disk(&name, &mounts);

        devices.push(DeviceInfo {
            id: name.clone(),
            path: format!("/dev/{name}"),
            name: if model.is_empty() {
                name.clone()
            } else {
                model
            },
            vendor,
            serial,
            bus,
            capacity,
            removable,
            system: system_parents.contains(&name),
            mounted: !related_mounts.is_empty(),
            mount_points: related_mounts,
        });
    }

    devices.sort_by_key(|device| (!device.removable, device.system, device.name.clone()));
    Ok(devices)
}

fn is_virtual_or_partition(name: &str, path: &Path) -> bool {
    if path.join("partition").exists() {
        return true;
    }
    ["loop", "ram", "zram", "dm-", "md", "sr", "fd"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}

fn detect_bus(path: &Path) -> String {
    let target = fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_lowercase();
    if target.contains("/usb") {
        "USB".into()
    } else if target.contains("/nvme") {
        "NVMe".into()
    } else if target.contains("/mmc") {
        "SD/MMC".into()
    } else if target.contains("/virtio") {
        "VirtIO".into()
    } else {
        "SATA/SCSI".into()
    }
}

fn read_trimmed(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn read_mounts() -> HashMap<String, Vec<String>> {
    let mut mounts: HashMap<String, Vec<String>> = HashMap::new();
    let Ok(contents) = fs::read_to_string("/proc/self/mountinfo") else {
        return mounts;
    };
    for line in contents.lines() {
        let Some(separator) = line.find(" - ") else {
            continue;
        };
        let left: Vec<&str> = line[..separator].split_whitespace().collect();
        let right: Vec<&str> = line[separator + 3..].split_whitespace().collect();
        if left.len() > 4 && right.len() > 1 && right[1].starts_with("/dev/") {
            mounts
                .entry(right[1].to_string())
                .or_default()
                .push(unescape_mount(left[4]));
        }
    }
    mounts
}

fn unescape_mount(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\134", "\\")
}

fn parent_disk(device_path: &str) -> Option<String> {
    let name = device_path.strip_prefix("/dev/")?;
    if name.starts_with("nvme") || name.starts_with("mmcblk") {
        return Some(
            name.rsplit_once('p')
                .map_or(name, |(base, _)| base)
                .to_string(),
        );
    }
    Some(
        name.trim_end_matches(|c: char| c.is_ascii_digit())
            .to_string(),
    )
}

fn system_disk_names(mounts: &HashMap<String, Vec<String>>) -> HashSet<String> {
    mounts
        .iter()
        .filter(|(_, points)| points.iter().any(|point| point == "/"))
        .filter_map(|(device, _)| parent_disk(device))
        .collect()
}

fn mounts_for_disk(name: &str, mounts: &HashMap<String, Vec<String>>) -> Vec<String> {
    mounts
        .iter()
        .filter(|(device, _)| parent_disk(device).as_deref() == Some(name))
        .flat_map(|(_, points)| points.iter().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parent_disk;

    #[test]
    fn finds_parent_disks() {
        assert_eq!(parent_disk("/dev/sdb1").as_deref(), Some("sdb"));
        assert_eq!(parent_disk("/dev/nvme0n1p2").as_deref(), Some("nvme0n1"));
        assert_eq!(parent_disk("/dev/mmcblk0p1").as_deref(), Some("mmcblk0"));
    }
}
