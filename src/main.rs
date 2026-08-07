#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod devices;
mod imaging;
mod theme;

use eframe::egui;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ALL ImgBackup")
            .with_inner_size([1240.0, 790.0])
            .with_min_inner_size([1000.0, 680.0])
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "ALL ImgBackup",
        options,
        Box::new(|cc| Ok(Box::new(app::AllImgBackupApp::new(cc)))),
    )
}

fn app_icon() -> egui::IconData {
    let size = 64usize;
    let mut rgba = vec![0u8; size * size * 4];
    let center = (size as f32 - 1.0) / 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            let idx = (y * size + x) * 4;
            if distance < 29.0 {
                let t = ((x + y) as f32 / (size * 2) as f32).clamp(0.0, 1.0);
                rgba[idx] = (55.0 + 70.0 * t) as u8;
                rgba[idx + 1] = (95.0 + 85.0 * t) as u8;
                rgba[idx + 2] = (235.0 + 20.0 * t) as u8;
                rgba[idx + 3] = 255;
            }
            if distance < 16.0 {
                rgba[idx] = 14;
                rgba[idx + 1] = 20;
                rgba[idx + 2] = 40;
                rgba[idx + 3] = 255;
            }
            if distance < 5.0 {
                rgba[idx] = 120;
                rgba[idx + 1] = 245;
                rgba[idx + 2] = 220;
                rgba[idx + 3] = 255;
            }
        }
    }
    egui::IconData {
        rgba,
        width: size as u32,
        height: size as u32,
    }
}
