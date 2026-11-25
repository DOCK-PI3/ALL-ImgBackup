use crate::devices::DeviceInfo;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::Sender,
    },
    thread,
    time::{Duration, Instant},
};

const BUFFER_SIZE: usize = 512 * 1024;
const MIN_RETRY_SIZE: usize = 64 * 1024;
const READ_RETRIES: usize = 3;

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
        let result = revalidate_device(&spec, &cancel).and_then(|()| match spec.kind {
            JobKind::Backup => backup(&spec, &cancel, &sender),
            JobKind::Restore => restore(&spec, &cancel, &sender),
        });
        match result {
            Ok((bytes, hash)) => {
                let _ = sender.send(JobEvent::Finished {
                    bytes,
                    sha256: hash,
                    elapsed: started.elapsed(),
                });
            }
            Err(error)
                if error.kind() == io::ErrorKind::Interrupted && cancel.load(Ordering::Relaxed) =>
            {
                let _ = sender.send(JobEvent::Cancelled);
            }
            Err(error) => {
                let _ = sender.send(JobEvent::Failed(friendly_error(&error, spec.kind)));
            }
        }
    });
}

fn revalidate_device(spec: &JobSpec, cancel: &AtomicBool) -> io::Result<()> {
    check_cancel(cancel)?;
    let devices = crate::devices::scan()
        .map_err(|error| io_context("no se pudieron comprobar los dispositivos", error))?;
    if spec.device.system || !devices.contains(&spec.device) {
        return Err(io::Error::other(
            "el dispositivo cambió o dejó de estar disponible. Actualiza la lista y vuelve a seleccionarlo",
        ));
    }
    if path_is_on_device(&spec.image, &spec.device) {
        return Err(io::Error::other(
            "el archivo de imagen está en el mismo dispositivo seleccionado",
        ));
    }
    check_cancel(cancel)
}

fn backup(
    spec: &JobSpec,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<(u64, String)> {
    sender.send(JobEvent::Stage(Stage::Preparing)).ok();
    check_cancel(cancel)?;
    let partial = partial_path(&spec.image);
    let output = OpenOptions::new()
        .create_new(true)
        .read(spec.verify)
        .write(true)
        .open(&partial)
        .map_err(|error| {
            io_context(
                &format!("no se pudo crear el archivo temporal {}", partial.display()),
                error,
            )
        })?;

    let result = (|| {
        let mut output = output;
        let (bytes, source_hash) = {
            #[cfg(windows)]
            let _volume_locks = lock_windows_volumes(&spec.device, false)?;
            let source = open_device_read(&spec.device.path).map_err(|error| {
                io_context(&format!("no se pudo abrir {}", spec.device.path), error)
            })?;
            sender.send(JobEvent::Stage(Stage::Copying)).ok();
            copy_exact(source, &mut output, spec.device.capacity, cancel, sender)
                .map_err(|error| io_context("copia de la unidad a la imagen", error))?
        };
        sender.send(JobEvent::Stage(Stage::Flushing)).ok();
        output
            .sync_all()
            .map_err(|error| io_context("sincronización de la imagen", error))?;

        if spec.verify {
            sender.send(JobEvent::Stage(Stage::Verifying)).ok();
            output.seek(SeekFrom::Start(0))?;
            let image_hash = hash_reader(&mut output, bytes, cancel, sender)
                .map_err(|error| io_context("verificación de la imagen", error))?;
            if image_hash != source_hash {
                return Err(io::Error::other("la verificación SHA-256 no coincide"));
            }
        }
        drop(output);
        check_cancel(cancel)?;
        // The temporary file is next to the final image, so replacement uses a single rename.
        fs::rename(&partial, &spec.image)
            .map_err(|error| io_context("no se pudo guardar la imagen final", error))?;
        Ok((bytes, source_hash))
    })();

    if let Err(error) = &result
        && let Err(cleanup_error) = fs::remove_file(&partial)
        && cleanup_error.kind() != io::ErrorKind::NotFound
    {
        return Err(io::Error::other(format!(
            "{error}. No se pudo eliminar {} ({cleanup_error})",
            partial.display()
        )));
    }
    result
}

fn restore(
    spec: &JobSpec,
    cancel: &AtomicBool,
    sender: &Sender<JobEvent>,
) -> io::Result<(u64, String)> {
    sender.send(JobEvent::Stage(Stage::Preparing)).ok();
    let source = File::open(&spec.image)?;
    let image_size = source.metadata()?.len();
    if image_size == 0 || !image_size.is_multiple_of(512) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "la imagen está vacía o su tamaño no está alineado a sectores de 512 bytes",
        ));
    }
    if image_size > spec.device.capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "la imagen es más grande que la capacidad del dispositivo",
        ));
    }

    #[cfg(target_os = "linux")]
    if spec.device.mounted {
        return Err(io::Error::other(
            "desmonta primero el dispositivo de destino",
        ));
    }
    check_cancel(cancel)?;
    let mut target = open_device_write(&spec.device)?;
    sender.send(JobEvent::Stage(Stage::Copying)).ok();
    let (bytes, image_hash) = copy_exact(source, &mut target, image_size, cancel, sender)
        .map_err(|error| io_context("copia de la imagen al dispositivo", error))?;

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

fn copy_exact<R: Read + Seek, W: Write>(
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

    while processed < total {
        let remaining = (total - processed).min(buffer.len() as u64) as usize;
        let count = read_chunk(&mut reader, &mut buffer[..remaining], processed, cancel)?;
        check_cancel(cancel)?;
        writer.write_all(&buffer[..count]).map_err(|error| {
            io_context(
                &format!("falló la escritura del bloque desde el byte {processed}"),
                error,
            )
        })?;
        hasher.update(&buffer[..count]);
        processed += count as u64;
        if last_update.elapsed() >= Duration::from_millis(120) || processed == total {
            send_progress(sender, processed, total, started);
            last_update = Instant::now();
        }
    }
    check_cancel(cancel)?;
    Ok((processed, format!("{:x}", hasher.finalize())))
}

fn read_chunk(
    reader: &mut (impl Read + Seek),
    buffer: &mut [u8],
    offset: u64,
    cancel: &AtomicBool,
) -> io::Result<usize> {
    let mut wanted = buffer.len();
    for attempt in 0..=READ_RETRIES {
        check_cancel(cancel)?;
        match reader.read(&mut buffer[..wanted]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!("la lectura terminó antes de tiempo en el byte {offset}"),
                ));
            }
            Ok(count) => return Ok(count),
            Err(error) if attempt < READ_RETRIES && retryable_read_error(&error) => {
                reader.seek(SeekFrom::Start(offset)).map_err(|error| {
                    io_context(
                        &format!("no se pudo retomar la lectura en el byte {offset}"),
                        error,
                    )
                })?;
                // Keep requests sector-aligned while reducing the transfer size after an I/O error.
                wanted = wanted.min((wanted / 2).next_multiple_of(4096).max(MIN_RETRY_SIZE));
            }
            Err(error) => {
                return Err(io_context(
                    &format!(
                        "falló la lectura del bloque desde el byte {offset} ({wanted} bytes, {} intento(s))",
                        attempt + 1
                    ),
                    error,
                ));
            }
        }
    }
    unreachable!("each final attempt returns a result")
}

fn retryable_read_error(error: &io::Error) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{ERROR_CRC, ERROR_IO_DEVICE, ERROR_SEM_TIMEOUT};
        if error.raw_os_error().is_some_and(|code| {
            matches!(code as u32, ERROR_CRC | ERROR_IO_DEVICE | ERROR_SEM_TIMEOUT)
        }) {
            return true;
        }
    }
    error.kind() == io::ErrorKind::Interrupted
}

fn check_cancel(cancel: &AtomicBool) -> io::Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "operación cancelada",
        ));
    }
    Ok(())
}

fn io_context(action: &str, error: io::Error) -> io::Error {
    io::Error::new(error.kind(), format!("{action} ({error})"))
}

fn hash_reader<R: Read + Seek>(
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
    while processed < total {
        let remaining = (total - processed).min(buffer.len() as u64) as usize;
        let count = read_chunk(&mut reader, &mut buffer[..remaining], processed, cancel)?;
        hasher.update(&buffer[..count]);
        processed += count as u64;
        if last_update.elapsed() >= Duration::from_millis(120) || processed == total {
            send_progress(sender, processed, total, started);
            last_update = Instant::now();
        }
    }
    check_cancel(cancel)?;
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
fn open_device_write(device: &DeviceInfo) -> io::Result<DeviceWriter> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_READ, FILE_SHARE_WRITE};
    let volume_locks = lock_windows_volumes(device, true)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .open(&device.path)?;
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
fn open_device_write(device: &DeviceInfo) -> io::Result<DeviceWriter> {
    Ok(DeviceWriter {
        file: OpenOptions::new()
            .read(true)
            .write(true)
            .open(&device.path)?,
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
fn lock_windows_volumes(device: &DeviceInfo, writable: bool) -> io::Result<Vec<File>> {
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
        // The output buffer and returned-byte count live for the synchronous call.
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
        // These control codes have no input or output buffer.
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

    let mut locked = Vec::new();
    if device.mount_points.is_empty() {
        return Ok(locked);
    }
    let disk_index = device
        .path
        .rsplit_once("PhysicalDrive")
        .and_then(|(_, value)| value.parse::<u32>().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "ruta de disco no válida"))?;
    for mount in &device.mount_points {
        let volume = mount.trim_end_matches('\\');
        let volume_path = format!(r"\\.\{volume}");
        let file = OpenOptions::new()
            .read(true)
            .write(writable)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .open(&volume_path)
            .map_err(|error| io_context(&format!("no se pudo abrir el volumen {volume}"), error))?;
        let extents = control::<VolumeExtents>(&file, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS)
            .map_err(|error| {
                io_context(&format!("no se pudo comprobar el volumen {volume}"), error)
            })?;
        if extents.count != 1 || extents.extent.disk_number != disk_index {
            return Err(io::Error::other(format!(
                "cambió el dispositivo del volumen {volume}"
            )));
        }
        control_no_output(&file, FSCTL_LOCK_VOLUME).map_err(|error| {
            io_context(
                &format!("no se pudo bloquear el volumen {volume}. Cierra las ventanas y los programas que lo estén usando"),
                error,
            )
        })?;
        control_no_output(&file, FSCTL_DISMOUNT_VOLUME).map_err(|error| {
            io_context(&format!("no se pudo desmontar el volumen {volume}"), error)
        })?;
        locked.push(file);
    }
    Ok(locked)
}

fn friendly_error(error: &io::Error, kind: JobKind) -> String {
    let action = match kind {
        JobKind::Backup => "crear la imagen",
        JobKind::Restore => "restaurar la imagen",
    };
    format!("No se pudo {action}: {error}")
}

pub fn path_is_on_device(path: &Path, device: &DeviceInfo) -> bool {
    let parent = path.parent().unwrap_or(path);
    let normalized_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    let normalized_file = path.canonicalize().ok();
    device.mount_points.iter().any(|mount| {
        let mount = Path::new(mount);
        let mount = mount.canonicalize().unwrap_or_else(|_| mount.to_path_buf());
        [
            Some(normalized_parent.as_path()),
            normalized_file.as_deref(),
        ]
        .into_iter()
        .flatten()
        .any(|candidate| {
            if cfg!(windows) {
                candidate
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .starts_with(&mount.to_string_lossy().to_ascii_lowercase())
            } else {
                candidate.starts_with(&mount)
            }
        })
    })
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
        io::Cursor,
        sync::mpsc,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn copies_and_hashes_exact_data() {
        let data = vec![0x5au8; 128 * 1024];
        let mut output = Vec::new();
        let cancel = AtomicBool::new(false);
        let (sender, _) = mpsc::channel();
        let (bytes, hash) = copy_exact(
            Cursor::new(&data),
            &mut output,
            data.len() as u64,
            &cancel,
            &sender,
        )
        .expect("copy should work");
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(data, output);
        assert_eq!(hash.len(), 64);
    }

    struct RetryReader {
        inner: Cursor<Vec<u8>>,
        requests: Vec<(u64, usize)>,
    }

    impl Read for RetryReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.requests.push((self.inner.position(), buffer.len()));
            if self.requests.len() == 1 {
                self.inner.set_position(self.inner.position() + 512);
                #[cfg(windows)]
                return Err(io::Error::from_raw_os_error(23));
                #[cfg(not(windows))]
                return Err(io::ErrorKind::Interrupted.into());
            }
            self.inner.read(buffer)
        }
    }

    impl Seek for RetryReader {
        fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
            self.inner.seek(position)
        }
    }

    #[test]
    fn retries_from_the_same_offset_with_a_smaller_transfer() {
        let data = (0..(BUFFER_SIZE + 512))
            .map(|i| i as u8)
            .collect::<Vec<_>>();
        let mut reader = RetryReader {
            inner: Cursor::new(data.clone()),
            requests: Vec::new(),
        };
        let mut output = Vec::new();
        let (sender, _) = mpsc::channel();
        copy_exact(
            &mut reader,
            &mut output,
            data.len() as u64,
            &AtomicBool::new(false),
            &sender,
        )
        .expect("transient read error must be recoverable");
        assert_eq!(
            reader.requests[..2],
            [(0, BUFFER_SIZE), (0, BUFFER_SIZE / 2)]
        );
        assert_eq!(output, data);
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
        let mut spec = JobSpec {
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
        let partial = partial_path(&image);
        fs::write(&image, b"previous image").expect("previous image");

        fs::write(&partial, b"another backup").expect("existing partial");
        assert_eq!(
            backup(&spec, &cancel, &sender)
                .expect_err("exclusive temp file")
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        assert_eq!(
            fs::read(&partial).expect("untouched partial"),
            b"another backup"
        );
        fs::remove_file(&partial).expect("remove test partial");

        spec.device.capacity += 512;
        assert_eq!(
            backup(&spec, &cancel, &sender)
                .expect_err("short source")
                .kind(),
            io::ErrorKind::UnexpectedEof,
        );
        assert_eq!(
            fs::read(&image).expect("untouched image"),
            b"previous image"
        );
        assert!(!partial.exists());
        spec.device.capacity -= 512;

        let (bytes, hash) = backup(&spec, &cancel, &sender).expect("verified backup");
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(hash.len(), 64);
        assert_eq!(fs::read(&image).expect("backup data"), data);
        assert!(!partial.exists());
        fs::remove_dir_all(directory).expect("cleanup test directory");
    }
}
