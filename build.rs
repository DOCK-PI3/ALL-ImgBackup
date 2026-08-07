fn main() {
    println!("cargo:rerun-if-changed=assets/app.manifest");
    println!("cargo:rerun-if-changed=assets/app-dev.manifest");

    #[cfg(windows)]
    {
        let manifest = if std::env::var("PROFILE").as_deref() == Ok("release") {
            "assets/app.manifest"
        } else {
            "assets/app-dev.manifest"
        };
        let output = std::path::PathBuf::from(
            std::env::var_os("OUT_DIR").expect("OUT_DIR no está definido"),
        );
        let icon = output.join("all-imgbackup.ico");
        std::fs::write(&icon, windows_icon()).expect("no se pudo generar el icono");

        let manifest = std::fs::canonicalize(manifest).expect("no se encontró el manifiesto");
        let resource = output.join("all-imgbackup.rc");
        let rc = format!(
            "1 ICON \"{}\"\n1 24 \"{}\"\n",
            icon.display().to_string().replace('\\', "/"),
            manifest.display().to_string().replace('\\', "/")
        );
        std::fs::write(&resource, rc).expect("no se pudo generar el archivo de recursos");

        embed_resource::compile(&resource, embed_resource::NONE)
            .manifest_required()
            .expect("no se pudieron incrustar los recursos de Windows");
    }
}

#[cfg(windows)]
fn windows_icon() -> Vec<u8> {
    const SIZE: usize = 64;
    const PIXEL_BYTES: usize = SIZE * SIZE * 4;
    const MASK_BYTES: usize = SIZE * 8;
    const IMAGE_BYTES: usize = 40 + PIXEL_BYTES + MASK_BYTES;
    let mut icon = Vec::with_capacity(22 + IMAGE_BYTES);

    push_u16(&mut icon, 0);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 1);
    icon.extend_from_slice(&[SIZE as u8, SIZE as u8, 0, 0]);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 32);
    push_u32(&mut icon, IMAGE_BYTES as u32);
    push_u32(&mut icon, 22);

    push_u32(&mut icon, 40);
    push_i32(&mut icon, SIZE as i32);
    push_i32(&mut icon, (SIZE * 2) as i32);
    push_u16(&mut icon, 1);
    push_u16(&mut icon, 32);
    push_u32(&mut icon, 0);
    push_u32(&mut icon, PIXEL_BYTES as u32);
    icon.extend_from_slice(&[0; 16]);

    let center = (SIZE as f32 - 1.0) / 2.0;
    for y in (0..SIZE).rev() {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let (red, green, blue, alpha) = if distance < 29.0 {
                let blend = (x + y) as f32 / (SIZE * 2) as f32;
                if distance < 5.0 {
                    (120, 245, 220, 255)
                } else if distance < 15.0 {
                    (14, 20, 40, 255)
                } else {
                    (
                        (55.0 + 70.0 * blend) as u8,
                        (95.0 + 85.0 * blend) as u8,
                        (235.0 + 20.0 * blend) as u8,
                        255,
                    )
                }
            } else {
                (0, 0, 0, 0)
            };
            icon.extend_from_slice(&[blue, green, red, alpha]);
        }
    }
    icon.resize(22 + IMAGE_BYTES, 0);
    icon
}

#[cfg(windows)]
fn push_u16(buffer: &mut Vec<u8>, value: u16) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

#[cfg(windows)]
fn push_u32(buffer: &mut Vec<u8>, value: u32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}

#[cfg(windows)]
fn push_i32(buffer: &mut Vec<u8>, value: i32) {
    buffer.extend_from_slice(&value.to_le_bytes());
}
