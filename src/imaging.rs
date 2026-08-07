use crate::devices::DeviceInfo;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

const BUFFER_SIZE: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobKind {
    Backup,
    Restore,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Preparing,
    Copying,
    Verifying,
    Flushing,
}

#[derive(Clone, Debug)]
pub struct JobSpec {
    pub kind: JobKind,
    pub device: DeviceInfo,
    pub image: PathBuf,
    pub verify: bool,
}

#[derive(Debug)]
pub enum JobEvent {
    Stage(Stage),
    Progress {
        processed: u64,
        total: u64,
        bytes_per_second: f64,
    },
    Finished {
        bytes: u64,
        sha256: String,
        elapsed: Duration,
    },
    Cancelled,
    Failed(String),
}

pub fn spawn(spec: JobSpec, cancel: Arc<AtomicBool>, sender: Sender<JobEvent>) {
    thread::spawn(move || {
        let started = Instant::now();
        let result = match spec.kind {
            JobKind::Backup => backup(&spec, &cancel, &sender),
            JobKind::Restore => restore(&spec, &cancel, &sender),
        };
        match result {
            Ok((bytes, hash)) if cancel.load(Ordering::Relaxed) => {
                let _ = sender.send(JobEvent::Cancelled);
                let _ = bytes;
                let _ = hash;
            }
            Ok((bytes, hash)) => {
                let _ = sender.send(JobEvent::Finished {
                    bytes,
                    sha256: hash,
                    elapsed: started.elapsed(),
                });
            }
            Err(error) if cancel.load(Ordering::Relaxed) => {
                let _ = sender.send(JobEvent::Cancelled);
                let _ = error;
            }
            Err(error) => {
                let _ = sender.send(JobEvent::Failed(friendly_error(&error, spec.kind)));
            }
        }
    });
}

fn backup(
    spec: &JobSpec,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<(u64, String)> {
    sender.send(JobEvent::Stage(Stage::Preparing)).ok();
    let source = open_device_read(&spec.device.path)?;
    let partial = partial_path(&spec.image);
    let output = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&partial)?;

    sender.send(JobEvent::Stage(Stage::Copying)).ok();
    let mut reader = BufReader::with_capacity(BUFFER_SIZE, source);
    let mut writer = BufWriter::with_capacity(BUFFER_SIZE, output);
    let copy_result = copy_exact(
        &mut reader,
        &mut writer,
        spec.device.capacity,
        cancel,
        sender,
    );

    let (bytes, source_hash) = match copy_result {
        Ok(value) => {
            writer.flush()?;
            writer.get_ref().sync_all()?;
            value
        }
        Err(error) => {
            drop(writer);
            drop(fs::remove_file(&partial));
            return Err(error);
        }
    };
    drop(writer);
    if cancel.load(Ordering::Relaxed) {
        drop(fs::remove_file(&partial));
        return Ok((bytes, source_hash));
    }

    if spec.verify {
        sender.send(JobEvent::Stage(Stage::Verifying)).ok();
        let image_hash = hash_file(&partial, bytes, cancel, sender)?;
        if image_hash != source_hash {
            drop(fs::remove_file(&partial));
            return Err(io::Error::other("la verificación SHA-256 no coincide"));
        }
    }

    if spec.image.exists() {
        fs::remove_file(&spec.image)?;
    }
    fs::rename(&partial, &spec.image)?;
    Ok((bytes, source_hash))
}

fn restore(
    spec: &JobSpec,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<(u64, String)> {
    sender.send(JobEvent::Stage(Stage::Preparing)).ok();
    let image_size = fs::metadata(&spec.image)?.len();
    if image_size > spec.device.capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "la imagen es más grande que la capacidad del dispositivo",
        ));
    }

    let source = File::open(&spec.image)?;
    let mut target = open_device_write(&spec.device.path)?;
    sender.send(JobEvent::Stage(Stage::Copying)).ok();
    let mut reader = BufReader::with_capacity(BUFFER_SIZE, source);
    let (bytes, image_hash) = copy_exact(&mut reader, &mut target, image_size, cancel, sender)?;

    if cancel.load(Ordering::Relaxed) {
        return Ok((bytes, image_hash));
    }

    sender.send(JobEvent::Stage(Stage::Flushing)).ok();
    target.flush()?;
    target.sync_all()?;

    if spec.verify {
        sender.send(JobEvent::Stage(Stage::Verifying)).ok();
        let read_target = open_device_read(&spec.device.path)?;
        let target_hash = hash_reader(read_target, image_size, cancel, sender)?;
        if target_hash != image_hash {
            return Err(io::Error::other(
                "la verificación del dispositivo no coincide",
            ));
        }
    }
    Ok((bytes, image_hash))
}

fn copy_exact<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    total: u64,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<(u64, String)> {
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut hasher = Sha256::new();
    let mut processed = 0u64;
    let started = Instant::now();
    let mut last_update = Instant::now();

    while processed < total && !cancel.load(Ordering::Relaxed) {
        let remaining = (total - processed).min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..remaining])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("lectura incompleta: {processed} de {total} bytes"),
            ));
        }
        writer.write_all(&buffer[..count])?;
        hasher.update(&buffer[..count]);
        processed += count as u64;
        if last_update.elapsed() >= Duration::from_millis(120) || processed == total {
            send_progress(sender, processed, total, started);
            last_update = Instant::now();
        }
    }
    writer.flush()?;
    Ok((processed, format!("{:x}", hasher.finalize())))
}

fn hash_file(
    path: &Path,
    total: u64,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<String> {
    hash_reader(File::open(path)?, total, cancel, sender)
}

fn hash_reader<R: Read>(
    mut reader: R,
    total: u64,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<String> {
    let mut buffer = vec![0u8; BUFFER_SIZE];
    let mut hasher = Sha256::new();
    let mut processed = 0u64;
    let started = Instant::now();
    let mut last_update = Instant::now();
    while processed < total && !cancel.load(Ordering::Relaxed) {
        let remaining = (total - processed).min(buffer.len() as u64) as usize;
        let count = reader.read(&mut buffer[..remaining])?;
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "lectura incompleta",
            ));
        }
        hasher.update(&buffer[..count]);
        processed += count as u64;
        if last_update.elapsed() >= Duration::from_millis(120) || processed == total {
            send_progress(sender, processed, total, started);
            last_update = Instant::now();
        }
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn send_progress(sender: &Sender<JobEvent>, processed: u64, total: u64, started: Instant) {
    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    sender
        .send(JobEvent::Progress {
            processed,
            total,
            bytes_per_second: processed as f64 / elapsed,
        })
        .ok();
}

fn partial_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(".part");
    PathBuf::from(value)
}

#[cfg(windows)]
fn open_device_read(path: &str) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)
}

#[cfg(windows)]
fn open_device_write(path: &str) -> io::Result<DeviceWriter> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let disk_index = path
        .rsplit_once("PhysicalDrive")
        .and_then(|(_, value)| value.parse::<u32>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ruta de disco no válida"))?;
    let volume_locks = lock_windows_volumes(disk_index)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(path)?;
    Ok(DeviceWriter {
        file,
        _volume_locks: volume_locks,
    })
}

#[cfg(not(windows))]
fn open_device_read(path: &str) -> io::Result<File> {
    File::open(path)
}

#[cfg(not(windows))]
fn open_device_write(path: &str) -> io::Result<DeviceWriter> {
    Ok(DeviceWriter {
        file: OpenOptions::new().read(true).write(true).open(path)?,
    })
}

struct DeviceWriter {
    file: File,
    #[cfg(windows)]
    _volume_locks: Vec<File>,
}

impl DeviceWriter {
    fn sync_all(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

impl Write for DeviceWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

#[cfg(windows)]
fn lock_windows_volumes(disk_index: u32) -> io::Result<Vec<File>> {
    use std::{
        ffi::c_void,
        mem,
        os::windows::{fs::OpenOptionsExt, io::AsRawHandle},
        ptr,
    };
    use windows_sys::Win32::{
        Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE},
        System::IO::DeviceIoControl,
    };

    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x00560000;
    const FSCTL_LOCK_VOLUME: u32 = 0x00090018;
    const FSCTL_DISMOUNT_VOLUME: u32 = 0x00090020;

    #[repr(C)]
    #[derive(Default, Copy, Clone)]
    struct DiskExtent {
        disk_number: u32,
        starting_offset: i64,
        extent_length: i64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct VolumeExtents {
        count: u32,
        extent: DiskExtent,
    }

    fn control<T: Default>(file: &File, code: u32) -> io::Result<T> {
        let mut output = T::default();
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                ptr::null(),
                0,
                &mut output as *mut T as *mut c_void,
                mem::size_of::<T>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(output)
        }
    }

    fn control_no_output(file: &File, code: u32) -> io::Result<()> {
        let mut returned = 0u32;
        let ok = unsafe {
            DeviceIoControl(
                file.as_raw_handle(),
                code,
                ptr::null(),
                0,
                ptr::null_mut(),
                0,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    let mut matching = Vec::new();
    for letter in b'A'..=b'Z' {
        let volume_path = format!(r"\\.\{}:", letter as char);
        let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&volume_path)
        else {
            continue;
        };
        let Ok(extents) = control::<VolumeExtents>(&file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS)
        else {
            continue;
        };
        if extents.extent.disk_number == disk_index {
            matching.push((letter as char, file));
        }
    }

    let mut locked = Vec::new();
    for (letter, file) in matching {
        control_no_output(&file, FSCTL_LOCK_VOLUME).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("no se pudo bloquear el volumen {letter}: ({error})"),
            )
        })?;
        control_no_output(&file, FSCTL_DISMOUNT_VOLUME).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("no se pudo desmontar el volumen {letter}: ({error})"),
            )
        })?;
        locked.push(file);
    }
    Ok(locked)
}

fn friendly_error(error: &io::Error, kind: JobKind) -> String {
    match error.kind() {
        io::ErrorKind::PermissionDenied => {
            if cfg!(windows) {
                "Permiso denegado. Cierra la aplicación y ejecútala como administrador.".into()
            } else {
                "Permiso denegado. Ejecuta ALL ImgBackup con sudo o configura permisos udev.".into()
            }
        }
        _ => format!(
            "No se pudo {}: {error}",
            if kind == JobKind::Backup {
                "crear la imagen"
            } else {
                "restaurar la imagen"
            }
        ),
    }
}

pub fn image_size(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|metadata| metadata.len())
}

pub fn estimate_remaining(processed: u64, total: u64, speed: f64) -> Option<Duration> {
    if speed <= 0.0 || processed >= total {
        None
    } else {
        Some(Duration::from_secs_f64((total - processed) as f64 / speed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::mpsc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn copies_and_hashes_exact_data() {
        let data = vec![0x5au8; 128 * 1024];
        let mut output = Vec::new();
        let cancel = AtomicBool::new(false);
        let (sender, _) = mpsc::channel();
        let (bytes, hash) = copy_exact(&data[..], &mut output, data.len() as u64, &cancel, &sender)
            .expect("copy should work");
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(data, output);
        assert_eq!(hash.len(), 64);
    }

    #[test]
    fn builds_partial_name() {
        assert_eq!(
            partial_path(Path::new("disk.img")),
            PathBuf::from("disk.img.part")
        );
    }

    #[test]
    fn creates_and_verifies_backup_transactionally() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "all-imgbackup-test-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("test directory");
        let source = directory.join("source.raw");
        let image = directory.join("backup.img");
        let data: Vec<u8> = (0..512 * 16).map(|index| (index % 251) as u8).collect();
        fs::write(&source, &data).expect("source data");
        let spec = JobSpec {
            kind: JobKind::Backup,
            device: DeviceInfo {
                id: "test".into(),
                path: source.display().to_string(),
                name: "Test disk".into(),
                vendor: String::new(),
                serial: String::new(),
                bus: "Test".into(),
                capacity: data.len() as u64,
                removable: true,
                system: false,
                mounted: false,
                mount_points: Vec::new(),
            },
            image: image.clone(),
            verify: true,
        };
        let cancel = AtomicBool::new(false);
        let (sender, _) = mpsc::channel();
        let (bytes, hash) = backup(&spec, &cancel, &sender).expect("verified backup");
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(hash.len(), 64);
        assert_eq!(fs::read(&image).expect("backup data"), data);
        assert!(!partial_path(&image).exists());
        fs::remove_dir_all(directory).expect("cleanup test directory");
    }
}
