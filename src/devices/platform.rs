use super::DeviceInfo;
use std::{ffi::c_void, io, mem, ptr};
use windows_sys::Win32::{
    Foundation::{CloseHandle, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE},
    Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING},
    System::{
        IO::DeviceIoControl,
        Ioctl::{
            IOCTL_DISK_GET_LENGTH_INFO, IOCTL_STORAGE_GET_HOTPLUG_INFO,
            IOCTL_STORAGE_QUERY_PROPERTY, PropertyStandardQuery, STORAGE_DEVICE_DESCRIPTOR,
            STORAGE_HOTPLUG_INFO, STORAGE_PROPERTY_QUERY, StorageDeviceProperty,
        },
    },
};

pub fn scan() -> io::Result<Vec<DeviceInfo>> {
    let mut devices = Vec::new();
    let system_disk = system_physical_drive();

    for index in 0..64 {
        let path = format!(r"\\.\PhysicalDrive{index}");
        let Ok(handle) = open_query(&path) else {
            continue;
        };

        let capacity = disk_length(handle).unwrap_or_default();
        if capacity == 0 {
            unsafe { CloseHandle(handle) };
            continue;
        }
        let descriptor = device_descriptor(handle).unwrap_or_default();
        let hotplug = hotplug_info(handle).unwrap_or_default();
        unsafe { CloseHandle(handle) };
        let mount_points = mounted_drive_letters(index);

        let model = descriptor.product.trim().to_string();
        devices.push(DeviceInfo {
            id: format!("PhysicalDrive{index}"),
            path: path.clone(),
            name: if model.is_empty() {
                format!("Disco físico {index}")
            } else {
                model
            },
            vendor: descriptor.vendor.trim().to_string(),
            serial: descriptor.serial.trim().to_string(),
            bus: descriptor.bus,
            capacity,
            removable: descriptor.removable || hotplug,
            system: system_disk == Some(index),
            mounted: !mount_points.is_empty(),
            mount_points,
        });
    }

    devices.sort_by_key(|device| (!device.removable, device.system, device.name.clone()));
    Ok(devices)
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn open_query(path: &str) -> io::Result<HANDLE> {
    let path = wide(path);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        Err(io::Error::last_os_error())
    } else {
        Ok(handle)
    }
}

fn ioctl<T>(handle: HANDLE, code: u32, input: *const c_void, input_size: u32) -> io::Result<T>
where
    T: Default,
{
    let mut output = T::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            code,
            input,
            input_size,
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

fn disk_length(handle: HANDLE) -> io::Result<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct LengthInformation {
        length: i64,
    }
    let value: LengthInformation = ioctl(handle, IOCTL_DISK_GET_LENGTH_INFO, ptr::null(), 0)?;
    Ok(value.length.max(0) as u64)
}

#[derive(Default)]
struct Descriptor {
    vendor: String,
    product: String,
    serial: String,
    bus: String,
    removable: bool,
}

fn device_descriptor(handle: HANDLE) -> io::Result<Descriptor> {
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    let mut buffer = vec![0u8; 4096];
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const c_void,
            mem::size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            &mut returned,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let raw = unsafe { &*(buffer.as_ptr() as *const STORAGE_DEVICE_DESCRIPTOR) };
    Ok(Descriptor {
        vendor: descriptor_string(&buffer, raw.VendorIdOffset),
        product: descriptor_string(&buffer, raw.ProductIdOffset),
        serial: descriptor_string(&buffer, raw.SerialNumberOffset),
        bus: bus_name(raw.BusType),
        removable: raw.RemovableMedia,
    })
}

fn descriptor_string(buffer: &[u8], offset: u32) -> String {
    let offset = offset as usize;
    if offset == 0 || offset >= buffer.len() {
        return String::new();
    }
    let end = buffer[offset..]
        .iter()
        .position(|byte| *byte == 0)
        .map_or(buffer.len(), |length| offset + length);
    String::from_utf8_lossy(&buffer[offset..end])
        .trim()
        .to_string()
}

fn hotplug_info(handle: HANDLE) -> io::Result<bool> {
    let info: STORAGE_HOTPLUG_INFO = ioctl(handle, IOCTL_STORAGE_GET_HOTPLUG_INFO, ptr::null(), 0)?;
    Ok(info.MediaRemovable || info.DeviceHotplug)
}

fn bus_name(value: i32) -> String {
    match value {
        1 => "SCSI",
        2 => "ATAPI",
        3 => "ATA",
        7 => "USB",
        11 => "SATA",
        12 => "SD",
        13 => "MMC",
        17 => "NVMe",
        _ => "Disco",
    }
    .into()
}

fn system_physical_drive() -> Option<u32> {
    // Consultar el volumen del sistema evita identificar el disco por heurísticas de tamaño/modelo.
    let root = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into());
    let volume = format!(r"\\.\{}", root.trim_end_matches('\\'));
    let path = wide(&volume);
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let result = volume_disk_number(handle);
    unsafe { CloseHandle(handle) };
    result.ok()
}

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

fn volume_disk_number(handle: HANDLE) -> io::Result<u32> {
    const IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS: u32 = 0x00560000;
    let value: VolumeExtents = ioctl(handle, IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, ptr::null(), 0)?;
    Ok(value.extent.disk_number)
}

fn mounted_drive_letters(disk_index: u32) -> Vec<String> {
    let mut letters = Vec::new();
    for letter in b'A'..=b'Z' {
        let path = format!(r"\\.\{}:", letter as char);
        let Ok(handle) = open_query(&path) else {
            continue;
        };
        let matches = volume_disk_number(handle).ok() == Some(disk_index);
        unsafe { CloseHandle(handle) };
        if matches {
            letters.push(format!("{}:\\", letter as char));
        }
    }
    letters
}
