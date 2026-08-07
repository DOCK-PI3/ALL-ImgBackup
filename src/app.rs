use crate::{
    devices::{self, DeviceInfo, format_bytes},
    imaging::{self, JobEvent, JobKind, JobSpec, Stage},
    theme,
};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Frame, Layout, Margin, Painter, Pos2, Rect, RichText,
    Sense, Stroke, Vec2,
};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Screen {
    Backup,
    Restore,
    Activity,
    About,
}

#[derive(Clone, Copy)]
enum UiIcon {
    Backup,
    Restore,
    Activity,
    Info,
    Refresh,
    Drive,
    Check,
    Warning,
    ArrowRight,
}

struct ActiveJob {
    kind: JobKind,
    device_name: String,
    receiver: Receiver<JobEvent>,
    cancel: Arc<AtomicBool>,
    stage: Stage,
    processed: u64,
    total: u64,
    speed: f64,
    started: Instant,
    cancelling: bool,
}

#[derive(Clone)]
struct ActivityEntry {
    title: String,
    detail: String,
    success: bool,
}

pub struct AllImgBackupApp {
    screen: Screen,
    devices: Vec<DeviceInfo>,
    selected_device: Option<String>,
    image_path: Option<PathBuf>,
    verify: bool,
    scan_error: Option<String>,
    scanning: bool,
    scan_receiver: Option<Receiver<Result<Vec<DeviceInfo>, String>>>,
    notice: Option<(String, bool)>,
    active_job: Option<ActiveJob>,
    activity: Vec<ActivityEntry>,
    restore_confirmation: bool,
    confirmation_text: String,
}

impl AllImgBackupApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::apply(&cc.egui_ctx);
        let mut app = Self {
            screen: Screen::Backup,
            devices: Vec::new(),
            selected_device: None,
            image_path: None,
            verify: true,
            scan_error: None,
            scanning: true,
            scan_receiver: None,
            notice: None,
            active_job: None,
            activity: Vec::new(),
            restore_confirmation: false,
            confirmation_text: String::new(),
        };
        app.refresh_devices();
        app
    }

    fn refresh_devices(&mut self) {
        self.scanning = true;
        self.scan_error = None;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let result = devices::scan().map_err(|error| error.to_string());
            sender.send(result).ok();
        });
        self.scan_receiver = Some(receiver);
    }

    fn poll_device_scan(&mut self) {
        let Some(receiver) = &self.scan_receiver else {
            return;
        };
        let result = match receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err(
                "El proceso de detección de dispositivos se interrumpió.".into(),
            )),
        };
        let Some(result) = result else {
            return;
        };
        self.scan_receiver = None;
        self.scanning = false;
        match result {
            Ok(devices) => {
                let selection_exists = self
                    .selected_device
                    .as_ref()
                    .is_some_and(|id| devices.iter().any(|device| &device.id == id));
                self.devices = devices;
                if !selection_exists {
                    self.selected_device = self
                        .devices
                        .iter()
                        .find(|device| device.removable && !device.system)
                        .or_else(|| self.devices.iter().find(|device| !device.system))
                        .map(|device| device.id.clone());
                }
                self.scan_error = None;
            }
            Err(error) => self.scan_error = Some(error),
        }
    }

    fn selected(&self) -> Option<&DeviceInfo> {
        let id = self.selected_device.as_ref()?;
        self.devices.iter().find(|device| &device.id == id)
    }

    fn choose_image(&mut self) {
        if self.active_job.is_some() {
            return;
        }
        let file = match self.screen {
            Screen::Backup => rfd::FileDialog::new()
                .set_title("Guardar imagen de disco")
                .set_file_name("backup.img")
                .add_filter("Imagen de disco RAW", &["img"])
                .save_file(),
            Screen::Restore => rfd::FileDialog::new()
                .set_title("Seleccionar imagen de disco")
                .add_filter("Imagen de disco RAW", &["img", "raw", "bin"])
                .pick_file(),
            _ => None,
        };
        if let Some(mut path) = file {
            if self.screen == Screen::Backup && path.extension().is_none() {
                path.set_extension("img");
            }
            self.image_path = Some(path);
            self.notice = None;
        }
    }

    fn validate(&self, kind: JobKind) -> Result<(DeviceInfo, PathBuf), String> {
        let device = self
            .selected()
            .cloned()
            .ok_or_else(|| "Selecciona un dispositivo.".to_string())?;
        if device.system {
            return Err("El disco del sistema está protegido y no puede utilizarse.".into());
        }
        let image = self
            .image_path
            .clone()
            .ok_or_else(|| "Selecciona la ruta del archivo de imagen.".to_string())?;
        if path_is_on_device(&image, &device) {
            return Err(
                "El archivo de imagen no puede estar en el mismo dispositivo seleccionado.".into(),
            );
        }

        if kind == JobKind::Restore {
            if device.mounted && cfg!(target_os = "linux") {
                return Err(format!(
                    "Desmonta primero todas las particiones del dispositivo: {}",
                    device.mount_points.join(", ")
                ));
            }
            let size = imaging::image_size(&image)
                .ok_or_else(|| "No se puede leer el archivo de imagen seleccionado.".to_string())?;
            if size == 0 {
                return Err("El archivo de imagen está vacío.".into());
            }
            if size % 512 != 0 {
                return Err(
                    "La imagen no tiene un tamaño alineado a sectores de 512 bytes.".into(),
                );
            }
            if size > device.capacity {
                return Err(format!(
                    "La imagen ({}) no cabe en el dispositivo ({}).",
                    format_bytes(size),
                    format_bytes(device.capacity)
                ));
            }
        } else if image.parent().is_none_or(|parent| !parent.exists()) {
            return Err("La carpeta de destino no existe.".into());
        }
        Ok((device, image))
    }

    fn request_start(&mut self, kind: JobKind) {
        match self.validate(kind) {
            Ok(_) if kind == JobKind::Restore => {
                self.confirmation_text.clear();
                self.restore_confirmation = true;
            }
            Ok(_) => self.start_job(kind),
            Err(message) => self.notice = Some((message, false)),
        }
    }

    fn start_job(&mut self, kind: JobKind) {
        let Ok((device, image)) = self.validate(kind) else {
            return;
        };
        let total = if kind == JobKind::Backup {
            device.capacity
        } else {
            imaging::image_size(&image).unwrap_or_default()
        };
        let (sender, receiver) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        imaging::spawn(
            JobSpec {
                kind,
                device: device.clone(),
                image,
                verify: self.verify,
            },
            cancel.clone(),
            sender,
        );
        self.notice = None;
        self.active_job = Some(ActiveJob {
            kind,
            device_name: device.name,
            receiver,
            cancel,
            stage: Stage::Preparing,
            processed: 0,
            total,
            speed: 0.0,
            started: Instant::now(),
            cancelling: false,
        });
    }

    fn poll_job(&mut self) {
        let mut terminal = None;
        if let Some(job) = &mut self.active_job {
            while let Ok(event) = job.receiver.try_recv() {
                match event {
                    JobEvent::Stage(stage) => {
                        job.stage = stage;
                        job.processed = 0;
                        job.speed = 0.0;
                    }
                    JobEvent::Progress {
                        processed,
                        total,
                        bytes_per_second,
                    } => {
                        job.processed = processed;
                        job.total = total;
                        job.speed = bytes_per_second;
                    }
                    event @ (JobEvent::Finished { .. }
                    | JobEvent::Cancelled
                    | JobEvent::Failed(_)) => terminal = Some(event),
                }
            }
        }

        if let Some(event) = terminal {
            let job = self.active_job.take().expect("active job");
            match event {
                JobEvent::Finished {
                    bytes,
                    sha256,
                    elapsed,
                } => {
                    let action = if job.kind == JobKind::Backup {
                        "Imagen creada"
                    } else {
                        "Imagen restaurada"
                    };
                    self.notice = Some((
                        format!(
                            "{action} correctamente · {} · {}",
                            format_bytes(bytes),
                            format_duration(elapsed)
                        ),
                        true,
                    ));
                    self.activity.insert(
                        0,
                        ActivityEntry {
                            title: format!("{action} — {}", job.device_name),
                            detail: format!("SHA-256  {}", &sha256[..16]),
                            success: true,
                        },
                    );
                }
                JobEvent::Cancelled => {
                    let message = if job.kind == JobKind::Backup {
                        "Backup cancelado. El archivo parcial se ha eliminado."
                    } else {
                        "Restauración cancelada. El dispositivo puede contener datos incompletos."
                    };
                    self.notice = Some((message.into(), false));
                    self.activity.insert(
                        0,
                        ActivityEntry {
                            title: format!("Operación cancelada — {}", job.device_name),
                            detail: format_duration(job.started.elapsed()),
                            success: false,
                        },
                    );
                }
                JobEvent::Failed(message) => {
                    self.notice = Some((message.clone(), false));
                    self.activity.insert(
                        0,
                        ActivityEntry {
                            title: format!("Error — {}", job.device_name),
                            detail: message,
                            success: false,
                        },
                    );
                }
                _ => {}
            }
        }
    }

    fn sidebar(&mut self, root: &mut egui::Ui) {
        egui::Panel::left("sidebar")
            .exact_size(232.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(10, 15, 29))
                    .inner_margin(20),
            )
            .show(root, |ui| {
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    draw_logo(ui, 42.0);
                    ui.vertical(|ui| {
                        ui.label(RichText::new("ALL").size(20.0).strong().color(theme::TEXT));
                        ui.label(RichText::new("IMG BACKUP").size(10.0).color(theme::CYAN));
                    });
                });
                ui.add_space(35.0);
                ui.label(RichText::new("HERRAMIENTAS").size(10.0).color(theme::MUTED));
                ui.add_space(4.0);
                nav_button(
                    ui,
                    &mut self.screen,
                    Screen::Backup,
                    UiIcon::Backup,
                    "Crear backup",
                );
                nav_button(
                    ui,
                    &mut self.screen,
                    Screen::Restore,
                    UiIcon::Restore,
                    "Restaurar imagen",
                );
                nav_button(
                    ui,
                    &mut self.screen,
                    Screen::Activity,
                    UiIcon::Activity,
                    "Actividad",
                );
                ui.add_space(18.0);
                ui.label(RichText::new("INFORMACIÓN").size(10.0).color(theme::MUTED));
                ui.add_space(4.0);
                nav_button(
                    ui,
                    &mut self.screen,
                    Screen::About,
                    UiIcon::Info,
                    "Acerca de",
                );

                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    Frame::new()
                        .fill(theme::SURFACE)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(14)
                        .inner_margin(14)
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let (rect, _) =
                                    ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
                                ui.painter().circle_filled(rect.center(), 4.0, theme::GREEN);
                                ui.label(
                                    RichText::new("Motor preparado")
                                        .size(12.0)
                                        .color(theme::TEXT),
                                );
                            });
                            ui.label(
                                RichText::new(if cfg!(windows) {
                                    "Windows · acceso RAW"
                                } else {
                                    "Linux · acceso RAW"
                                })
                                .size(11.0)
                                .color(theme::MUTED),
                            );
                        });
                });
            });
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.vertical(|ui| match self.screen {
                Screen::Backup => {
                    ui.heading(RichText::new("Crear una imagen").color(theme::TEXT));
                    ui.label(
                        RichText::new("Conserva una copia exacta de tu dispositivo")
                            .color(theme::MUTED),
                    );
                }
                Screen::Restore => {
                    ui.heading(RichText::new("Restaurar una imagen").color(theme::TEXT));
                    ui.label(
                        RichText::new("Graba una imagen RAW en una unidad física")
                            .color(theme::MUTED),
                    );
                }
                Screen::Activity => {
                    ui.heading(RichText::new("Actividad").color(theme::TEXT));
                    ui.label(
                        RichText::new("Resultado de las operaciones de esta sesión")
                            .color(theme::MUTED),
                    );
                }
                Screen::About => {
                    ui.heading(RichText::new("Acerca de ALL ImgBackup").color(theme::TEXT));
                    ui.label(
                        RichText::new("Imágenes de disco, sin capas innecesarias")
                            .color(theme::MUTED),
                    );
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let refresh = ui
                    .add_enabled_ui(self.active_job.is_none() && !self.scanning, |ui| {
                        outline_icon_button(
                            ui,
                            UiIcon::Refresh,
                            if self.scanning {
                                "Buscando..."
                            } else {
                                "Actualizar discos"
                            },
                            174.0,
                        )
                    })
                    .inner;
                if refresh.clicked() {
                    self.refresh_devices();
                }
            });
        });
        ui.add_space(22.0);
    }

    fn workflow(&mut self, ui: &mut egui::Ui, kind: JobKind) {
        if let Some(job) = &self.active_job {
            self.job_progress(ui, job);
            return;
        }

        let source_title = if kind == JobKind::Backup {
            "1   Dispositivo de origen"
        } else {
            "2   Dispositivo de destino"
        };
        section_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(source_title)
                        .size(17.0)
                        .strong()
                        .color(theme::TEXT),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(
                        RichText::new(format!("{} detectado(s)", self.devices.len()))
                            .size(12.0)
                            .color(theme::MUTED),
                    );
                });
            });
            ui.add_space(10.0);
            if self.devices.is_empty() {
                if self.scanning {
                    scanning_devices(ui);
                } else {
                    empty_devices(ui, self.scan_error.as_deref());
                }
            } else {
                egui::ScrollArea::vertical()
                    .id_salt("device_list")
                    .max_height(216.0)
                    .show(ui, |ui| {
                        for device in &self.devices {
                            let selected = self.selected_device.as_deref() == Some(&device.id);
                            let enabled = !device.system;
                            let response = device_row(ui, device, selected, enabled);
                            if response.clicked() && enabled {
                                self.selected_device = Some(device.id.clone());
                                self.notice = None;
                            }
                        }
                    });
            }
        });

        ui.add_space(14.0);
        let file_title = if kind == JobKind::Backup {
            "2   Archivo de destino"
        } else {
            "1   Imagen de origen"
        };
        section_card(ui, |ui| {
            ui.label(
                RichText::new(file_title)
                    .size(17.0)
                    .strong()
                    .color(theme::TEXT),
            );
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                let path_text = self
                    .image_path
                    .as_deref()
                    .map(compact_path)
                    .unwrap_or_else(|| {
                        if kind == JobKind::Backup {
                            "Elige dónde guardar el archivo .img".into()
                        } else {
                            "Elige un archivo .img para restaurar".into()
                        }
                    });
                Frame::new()
                    .fill(Color32::from_rgb(9, 14, 27))
                    .stroke(Stroke::new(1.0, theme::BORDER))
                    .corner_radius(10)
                    .inner_margin(Margin::symmetric(14, 11))
                    .show(ui, |ui| {
                        ui.set_min_width((ui.available_width() - 150.0).max(260.0));
                        ui.label(
                            RichText::new(path_text).color(if self.image_path.is_some() {
                                theme::TEXT
                            } else {
                                theme::MUTED
                            }),
                        );
                    });
                if ui.button("Examinar…").clicked() {
                    self.choose_image();
                }
            });
            if kind == JobKind::Restore
                && let Some(path) = &self.image_path
                && let Some(size) = imaging::image_size(path)
            {
                ui.label(
                    RichText::new(format!("Imagen RAW · {}", format_bytes(size)))
                        .size(12.0)
                        .color(theme::MUTED),
                );
            }
        });

        if kind == JobKind::Backup && self.selected().is_some_and(|device| device.mounted) {
            ui.add_space(10.0);
            notice(
                ui,
                "Cierra los programas que estén usando esta unidad para obtener una copia coherente.",
                false,
            );
        }

        ui.add_space(14.0);
        section_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Verificación SHA-256")
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new("Comprueba byte a byte el resultado al terminar")
                            .size(12.0)
                            .color(theme::MUTED),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let toggle_text = if self.verify {
                        "Activada"
                    } else {
                        "Desactivada"
                    };
                    ui.toggle_value(&mut self.verify, toggle_text);
                });
            });
        });

        if let Some((message, success)) = &self.notice {
            ui.add_space(12.0);
            notice(ui, message, *success);
        }

        ui.add_space(16.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let label = if kind == JobKind::Backup {
                "Crear imagen"
            } else {
                "Revisar y restaurar"
            };
            let color = if kind == JobKind::Backup {
                theme::BLUE
            } else {
                theme::RED
            };
            if filled_icon_button(ui, UiIcon::ArrowRight, label, color, 220.0).clicked() {
                self.request_start(kind);
            }
            ui.label(
                RichText::new("Se requieren privilegios de administrador")
                    .size(11.0)
                    .color(theme::MUTED),
            );
        });
    }

    fn job_progress(&self, ui: &mut egui::Ui, job: &ActiveJob) {
        let fraction = if job.total == 0 {
            0.0
        } else {
            job.processed as f32 / job.total as f32
        }
        .clamp(0.0, 1.0);
        ui.add_space(20.0);
        ui.vertical_centered(|ui| {
            draw_progress_orbit(ui, fraction, job.kind);
            ui.add_space(16.0);
            ui.heading(
                RichText::new(stage_title(job.stage, job.kind))
                    .size(25.0)
                    .color(theme::TEXT),
            );
            ui.label(RichText::new(&job.device_name).color(theme::MUTED));
        });
        ui.add_space(25.0);
        section_card(ui, |ui| {
            let bar = egui::ProgressBar::new(fraction)
                .desired_height(13.0)
                .fill(if job.kind == JobKind::Backup {
                    theme::BLUE
                } else {
                    theme::CYAN
                })
                .corner_radius(7);
            ui.add(bar);
            ui.add_space(12.0);
            ui.columns(3, |columns| {
                metric(
                    &mut columns[0],
                    "PROGRESO",
                    &format!("{:.1}%", fraction * 100.0),
                );
                metric(
                    &mut columns[1],
                    "VELOCIDAD",
                    &format!("{}/s", format_bytes(job.speed as u64)),
                );
                let remaining = imaging::estimate_remaining(job.processed, job.total, job.speed)
                    .map(format_duration)
                    .unwrap_or_else(|| "Calculando…".into());
                metric(&mut columns[2], "TIEMPO RESTANTE", &remaining);
            });
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!(
                    "{} de {} procesados",
                    format_bytes(job.processed),
                    format_bytes(job.total)
                ))
                .size(12.0)
                .color(theme::MUTED),
            );
        });
    }

    fn activity_screen(&self, ui: &mut egui::Ui) {
        if self.activity.is_empty() {
            ui.add_space(80.0);
            ui.vertical_centered(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(58.0), Sense::hover());
                paint_icon(
                    ui.painter(),
                    rect.shrink(7.0),
                    UiIcon::Activity,
                    theme::BORDER,
                    2.2,
                );
                ui.add_space(10.0);
                ui.label(
                    RichText::new("Todavía no hay actividad")
                        .size(20.0)
                        .color(theme::TEXT),
                );
                ui.label(
                    RichText::new("Las operaciones completadas aparecerán aquí")
                        .color(theme::MUTED),
                );
            });
            return;
        }
        for entry in &self.activity {
            section_card(ui, |ui| {
                ui.horizontal(|ui| {
                    let color = if entry.success {
                        theme::GREEN
                    } else {
                        theme::AMBER
                    };
                    let icon = if entry.success {
                        UiIcon::Check
                    } else {
                        UiIcon::Warning
                    };
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::hover());
                    paint_icon(ui.painter(), rect.shrink(2.0), icon, color, 1.8);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&entry.title).strong().color(theme::TEXT));
                        ui.label(RichText::new(&entry.detail).size(12.0).color(theme::MUTED));
                    });
                });
            });
            ui.add_space(10.0);
        }
    }

    fn about_screen(&self, ui: &mut egui::Ui) {
        section_card(ui, |ui| {
            ui.horizontal(|ui| {
                draw_logo(ui, 74.0);
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("ALL ImgBackup")
                            .size(25.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(RichText::new("Versión 0.1.0 · motor nativo Rust").color(theme::CYAN));
                    ui.add_space(5.0);
                    ui.label(
                        RichText::new("Compatible con imágenes RAW sector por sector.")
                            .color(theme::MUTED),
                    );
                });
            });
        });
        ui.add_space(14.0);
        ui.columns(2, |columns| {
            info_card(
                &mut columns[0],
                "SIN TECNOLOGÍAS WEB",
                "Interfaz nativa acelerada por GPU. Sin navegador incrustado, HTML ni JavaScript.",
            );
            info_card(
                &mut columns[1],
                "FORMATO ABIERTO",
                "Archivos .img compatibles con Win32 Disk Imager, dd y herramientas equivalentes.",
            );
        });
        ui.add_space(12.0);
        ui.columns(2, |columns| {
            info_card(&mut columns[0], "SEGURIDAD", "Bloqueo del disco del sistema, confirmación destructiva y archivo parcial para backups.");
            info_card(&mut columns[1], "MULTIPLATAFORMA", "Mismo proyecto y experiencia de uso en Windows 10/11 y distribuciones Linux modernas.");
        });
    }

    fn confirm_window(&mut self, ctx: &egui::Context) {
        if !self.restore_confirmation {
            return;
        }
        let mut close = false;
        let mut confirm = false;
        egui::Window::new("Confirmar restauración")
            .anchor(Align2::CENTER_CENTER, Vec2::ZERO)
            .collapsible(false)
            .resizable(false)
            .movable(false)
            .title_bar(false)
            .frame(
                Frame::window(&ctx.global_style())
                    .fill(theme::SURFACE)
                    .stroke(Stroke::new(1.0, theme::RED.gamma_multiply(0.7)))
                    .corner_radius(18)
                    .inner_margin(24),
            )
            .show(ctx, |ui| {
                ui.set_width(480.0);
                ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
                    paint_icon(
                        ui.painter(),
                        rect.shrink(2.0),
                        UiIcon::Warning,
                        theme::RED,
                        2.2,
                    );
                    ui.label(
                        RichText::new("Vas a sobrescribir un disco")
                            .size(22.0)
                            .strong()
                            .color(theme::TEXT),
                    );
                });
                ui.add_space(8.0);
                if let Some(device) = self.selected() {
                    ui.label(
                        RichText::new(format!(
                            "Se eliminarán todos los datos de {} ({}, {}).",
                            device.name,
                            format_bytes(device.capacity),
                            device.path
                        ))
                        .color(theme::MUTED),
                    );
                }
                ui.add_space(16.0);
                ui.label(
                    RichText::new("Escribe BORRAR para confirmar:")
                        .strong()
                        .color(theme::TEXT),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.confirmation_text)
                        .hint_text("BORRAR")
                        .desired_width(f32::INFINITY),
                );
                ui.add_space(16.0);
                ui.horizontal(|ui| {
                    if ui.button("Cancelar").clicked() {
                        close = true;
                    }
                    let allowed = self.confirmation_text.trim() == "BORRAR";
                    let button = egui::Button::new(RichText::new("Restaurar ahora").strong())
                        .fill(theme::RED)
                        .corner_radius(10);
                    if ui.add_enabled(allowed, button).clicked() {
                        confirm = true;
                    }
                });
            });
        if close {
            self.restore_confirmation = false;
            self.confirmation_text.clear();
        }
        if confirm {
            self.restore_confirmation = false;
            self.start_job(JobKind::Restore);
        }
    }
}

impl eframe::App for AllImgBackupApp {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root.ctx().clone();
        self.poll_device_scan();
        self.poll_job();
        if self.active_job.is_some() || self.scanning {
            ctx.request_repaint_after(Duration::from_millis(75));
        }

        self.sidebar(root);
        if let Some(job) = &mut self.active_job {
            egui::Panel::bottom("cancel_bar")
                .exact_size(64.0)
                .show_separator_line(false)
                .frame(
                    Frame::new()
                        .fill(Color32::from_rgb(10, 15, 29))
                        .inner_margin(Margin::symmetric(34, 10)),
                )
                .show(root, |ui| {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let text = if job.cancelling {
                            "Cancelando…"
                        } else {
                            "Cancelar operación"
                        };
                        if ui
                            .add_enabled(!job.cancelling, egui::Button::new(text))
                            .clicked()
                        {
                            job.cancel.store(true, Ordering::Relaxed);
                            job.cancelling = true;
                        }
                        ui.label(
                            RichText::new(
                                "No desconectes el dispositivo mientras la operación esté activa",
                            )
                            .size(12.0)
                            .color(theme::MUTED),
                        );
                    });
                });
        }
        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(theme::BG)
                    .inner_margin(Margin::symmetric(34, 26)),
            )
            .show(root, |ui| {
                paint_ambient(ui);
                self.top_bar(ui);
                egui::ScrollArea::vertical()
                    .id_salt("main_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| match self.screen {
                        Screen::Backup => self.workflow(ui, JobKind::Backup),
                        Screen::Restore => self.workflow(ui, JobKind::Restore),
                        Screen::Activity => self.activity_screen(ui),
                        Screen::About => self.about_screen(ui),
                    });
            });

        self.confirm_window(&ctx);
    }
}

fn nav_button(ui: &mut egui::Ui, current: &mut Screen, target: Screen, icon: UiIcon, label: &str) {
    let selected = *current == target;
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(ui.available_width(), 45.0), Sense::click());
    let hovered = response.hovered();
    let fill = if selected {
        theme::BLUE.gamma_multiply(0.28)
    } else if hovered {
        theme::SURFACE_2
    } else {
        Color32::TRANSPARENT
    };
    ui.painter().rect(
        rect,
        10,
        fill,
        if selected {
            Stroke::new(1.0, theme::BLUE.gamma_multiply(0.55))
        } else {
            Stroke::NONE
        },
        egui::StrokeKind::Inside,
    );
    let color = if selected { theme::TEXT } else { theme::MUTED };
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 23.0, rect.center().y),
        Vec2::splat(20.0),
    );
    paint_icon(ui.painter(), icon_rect, icon, color, 1.8);
    ui.painter().text(
        Pos2::new(rect.left() + 45.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.0),
        color,
    );
    if response.clicked() {
        *current = target;
    }
}

fn section_card(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(theme::SURFACE.gamma_multiply(0.96))
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(15)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            content(ui);
        });
}

fn device_row(
    ui: &mut egui::Ui,
    device: &DeviceInfo,
    selected: bool,
    enabled: bool,
) -> egui::Response {
    let fill = if selected {
        theme::BLUE.gamma_multiply(0.19)
    } else {
        Color32::from_rgb(11, 17, 31)
    };
    let stroke = if selected {
        Stroke::new(1.4, theme::BLUE)
    } else {
        Stroke::new(1.0, theme::BORDER)
    };
    let shown = Frame::new()
        .fill(fill)
        .stroke(stroke)
        .corner_radius(11)
        .inner_margin(Margin::symmetric(14, 11))
        .show(ui, |ui| {
            ui.set_min_width(ui.available_width());
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), Sense::hover());
                ui.painter().circle_filled(
                    rect.center(),
                    19.0,
                    if selected {
                        theme::BLUE.gamma_multiply(0.35)
                    } else {
                        theme::SURFACE_2
                    },
                );
                paint_icon(
                    ui.painter(),
                    rect.shrink(9.0),
                    UiIcon::Drive,
                    if selected { theme::CYAN } else { theme::MUTED },
                    1.7,
                );
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&device.name).strong().color(if enabled {
                            theme::TEXT
                        } else {
                            theme::MUTED
                        }));
                        if device.system {
                            badge(ui, "SISTEMA", theme::RED);
                        } else if device.removable {
                            badge(ui, "EXTRAÍBLE", theme::CYAN);
                        }
                    });
                    ui.label(
                        RichText::new(device.subtitle())
                            .size(12.0)
                            .color(theme::MUTED),
                    );
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let (radio, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                    ui.painter().circle_stroke(
                        radio.center(),
                        7.0,
                        Stroke::new(1.5, if selected { theme::CYAN } else { theme::BORDER }),
                    );
                    if selected {
                        ui.painter().circle_filled(radio.center(), 3.7, theme::CYAN);
                    }
                });
            });
        });
    shown.response.interact(Sense::click())
}

fn badge(ui: &mut egui::Ui, text: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.14))
        .corner_radius(5)
        .inner_margin(Margin::symmetric(6, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(text).size(9.0).strong().color(color));
        });
}

fn empty_devices(ui: &mut egui::Ui, error: Option<&str>) {
    Frame::new()
        .fill(Color32::from_rgb(10, 16, 29))
        .corner_radius(10)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.label(
                RichText::new("No se encontraron dispositivos compatibles")
                    .strong()
                    .color(theme::TEXT),
            );
            ui.label(
                RichText::new(error.unwrap_or("Conecta una unidad y pulsa “Actualizar discos”."))
                    .color(theme::MUTED),
            );
        });
}

fn scanning_devices(ui: &mut egui::Ui) {
    Frame::new()
        .fill(Color32::from_rgb(10, 16, 29))
        .corner_radius(10)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.vertical(|ui| {
                    ui.label(
                        RichText::new("Buscando dispositivos…")
                            .strong()
                            .color(theme::TEXT),
                    );
                    ui.label(
                        RichText::new("Consultando el hardware de almacenamiento")
                            .color(theme::MUTED),
                    );
                });
            });
        });
}

fn notice(ui: &mut egui::Ui, message: &str, success: bool) {
    let color = if success { theme::GREEN } else { theme::AMBER };
    Frame::new()
        .fill(color.gamma_multiply(0.10))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.45)))
        .corner_radius(10)
        .inner_margin(Margin::symmetric(13, 10))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::hover());
                paint_icon(
                    ui.painter(),
                    rect.shrink(2.0),
                    if success {
                        UiIcon::Check
                    } else {
                        UiIcon::Warning
                    },
                    color,
                    1.8,
                );
                ui.label(RichText::new(message).color(color));
            });
        });
}

fn outline_icon_button(ui: &mut egui::Ui, icon: UiIcon, label: &str, width: f32) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 42.0), Sense::click());
    let enabled = ui.is_enabled();
    let foreground = if enabled {
        theme::TEXT
    } else {
        theme::MUTED.gamma_multiply(0.55)
    };
    let fill = if response.hovered() && enabled {
        theme::SURFACE_2
    } else {
        theme::SURFACE
    };
    ui.painter().rect(
        rect,
        10,
        fill,
        Stroke::new(
            1.0,
            if response.hovered() && enabled {
                theme::BLUE
            } else {
                theme::BORDER
            },
        ),
        egui::StrokeKind::Inside,
    );
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 23.0, rect.center().y),
        Vec2::splat(18.0),
    );
    paint_icon(ui.painter(), icon_rect, icon, foreground, 1.7);
    ui.painter().text(
        Pos2::new(rect.left() + 42.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        foreground,
    );
    response
}

fn filled_icon_button(
    ui: &mut egui::Ui,
    icon: UiIcon,
    label: &str,
    color: Color32,
    width: f32,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 48.0), Sense::click());
    let fill = if response.is_pointer_button_down_on() {
        color.gamma_multiply(0.78)
    } else if response.hovered() {
        color.gamma_multiply(1.12)
    } else {
        color
    };
    ui.painter().rect_filled(rect, 11, fill);
    ui.painter().text(
        Pos2::new(rect.left() + 18.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(15.0),
        Color32::WHITE,
    );
    let icon_rect = Rect::from_center_size(
        Pos2::new(rect.right() - 22.0, rect.center().y),
        Vec2::splat(18.0),
    );
    paint_icon(ui.painter(), icon_rect, icon, Color32::WHITE, 2.0);
    response
}

fn paint_icon(painter: &Painter, rect: Rect, icon: UiIcon, color: Color32, width: f32) {
    let center = rect.center();
    let stroke = Stroke::new(width, color);
    let left = rect.left();
    let right = rect.right();
    let top = rect.top();
    let bottom = rect.bottom();
    match icon {
        UiIcon::Backup | UiIcon::Restore => {
            let upward = matches!(icon, UiIcon::Restore);
            let direction = if upward { -1.0 } else { 1.0 };
            let tip_y = center.y + direction * rect.height() * 0.20;
            let tail_y = center.y - direction * rect.height() * 0.32;
            painter.line_segment(
                [Pos2::new(center.x, tail_y), Pos2::new(center.x, tip_y)],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, tip_y),
                    Pos2::new(
                        center.x - rect.width() * 0.20,
                        tip_y - direction * rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, tip_y),
                    Pos2::new(
                        center.x + rect.width() * 0.20,
                        tip_y - direction * rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
            let tray_y = if upward { bottom - 1.0 } else { bottom - 2.0 };
            painter.line_segment(
                [
                    Pos2::new(left + rect.width() * 0.15, tray_y - rect.height() * 0.18),
                    Pos2::new(left + rect.width() * 0.15, tray_y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(left + rect.width() * 0.15, tray_y),
                    Pos2::new(right - rect.width() * 0.15, tray_y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(right - rect.width() * 0.15, tray_y),
                    Pos2::new(right - rect.width() * 0.15, tray_y - rect.height() * 0.18),
                ],
                stroke,
            );
        }
        UiIcon::Activity => {
            painter.circle_stroke(center, rect.width().min(rect.height()) * 0.42, stroke);
            painter.line_segment(
                [center, Pos2::new(center.x, top + rect.height() * 0.25)],
                stroke,
            );
            painter.line_segment(
                [center, Pos2::new(right - rect.width() * 0.22, center.y)],
                stroke,
            );
        }
        UiIcon::Info => {
            painter.circle_stroke(center, rect.width().min(rect.height()) * 0.42, stroke);
            painter.circle_filled(
                Pos2::new(center.x, top + rect.height() * 0.27),
                width.max(1.5),
                color,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, center.y - rect.height() * 0.02),
                    Pos2::new(center.x, bottom - rect.height() * 0.22),
                ],
                stroke,
            );
        }
        UiIcon::Refresh => {
            let radius = rect.width().min(rect.height()) * 0.36;
            let points: Vec<Pos2> = (0..=18)
                .map(|step| {
                    let angle = -2.4 + step as f32 / 18.0 * 4.65;
                    center + Vec2::angled(angle) * radius
                })
                .collect();
            painter.add(egui::Shape::line(points, stroke));
            let tip = center + Vec2::angled(2.25) * radius;
            painter.line_segment([tip, tip + Vec2::new(-1.0, -6.0)], stroke);
            painter.line_segment([tip, tip + Vec2::new(-6.0, 1.0)], stroke);
        }
        UiIcon::Drive => {
            painter.rect_stroke(rect, 3, stroke, egui::StrokeKind::Inside);
            let separator_y = top + rect.height() * 0.62;
            painter.line_segment(
                [Pos2::new(left, separator_y), Pos2::new(right, separator_y)],
                stroke,
            );
            painter.circle_filled(
                Pos2::new(right - rect.width() * 0.20, bottom - rect.height() * 0.18),
                width.max(1.4),
                color,
            );
        }
        UiIcon::Check => {
            painter.add(egui::Shape::line(
                vec![
                    Pos2::new(left + rect.width() * 0.10, center.y),
                    Pos2::new(
                        center.x - rect.width() * 0.08,
                        bottom - rect.height() * 0.18,
                    ),
                    Pos2::new(right - rect.width() * 0.08, top + rect.height() * 0.16),
                ],
                stroke,
            ));
        }
        UiIcon::Warning => {
            let triangle = vec![
                Pos2::new(center.x, top + rect.height() * 0.06),
                Pos2::new(right - rect.width() * 0.04, bottom - rect.height() * 0.06),
                Pos2::new(left + rect.width() * 0.04, bottom - rect.height() * 0.06),
                Pos2::new(center.x, top + rect.height() * 0.06),
            ];
            painter.add(egui::Shape::line(triangle, stroke));
            painter.line_segment(
                [
                    Pos2::new(center.x, top + rect.height() * 0.34),
                    Pos2::new(center.x, center.y + rect.height() * 0.10),
                ],
                stroke,
            );
            painter.circle_filled(
                Pos2::new(center.x, bottom - rect.height() * 0.24),
                width.max(1.3),
                color,
            );
        }
        UiIcon::ArrowRight => {
            painter.line_segment(
                [
                    Pos2::new(left + rect.width() * 0.12, center.y),
                    Pos2::new(right - rect.width() * 0.12, center.y),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(right - rect.width() * 0.12, center.y),
                    Pos2::new(center.x + rect.width() * 0.10, top + rect.height() * 0.18),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(right - rect.width() * 0.12, center.y),
                    Pos2::new(
                        center.x + rect.width() * 0.10,
                        bottom - rect.height() * 0.18,
                    ),
                ],
                stroke,
            );
        }
    }
}

fn metric(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.vertical_centered(|ui| {
        ui.label(RichText::new(label).size(10.0).color(theme::MUTED));
        ui.label(RichText::new(value).size(18.0).strong().color(theme::TEXT));
    });
}

fn info_card(ui: &mut egui::Ui, title: &str, text: &str) {
    Frame::new()
        .fill(theme::SURFACE)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(14)
        .inner_margin(18)
        .show(ui, |ui| {
            ui.set_min_height(90.0);
            ui.label(RichText::new(title).size(11.0).strong().color(theme::CYAN));
            ui.add_space(6.0);
            ui.label(RichText::new(text).color(theme::MUTED));
        });
}

fn draw_logo(ui: &mut egui::Ui, size: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let center = rect.center();
    ui.painter()
        .circle_filled(center, size * 0.48, theme::BLUE.gamma_multiply(0.25));
    ui.painter().circle_stroke(
        center,
        size * 0.36,
        Stroke::new((size / 18.0).max(2.0), theme::BLUE),
    );
    ui.painter().circle_stroke(
        center,
        size * 0.26,
        Stroke::new((size / 30.0).max(1.0), theme::CYAN),
    );
    ui.painter().circle_filled(center, size * 0.07, theme::CYAN);
}

fn draw_progress_orbit(ui: &mut egui::Ui, fraction: f32, kind: JobKind) {
    let size = 150.0;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let center = rect.center();
    let radius = 62.0;
    ui.painter()
        .circle_stroke(center, radius, Stroke::new(9.0, theme::SURFACE_2));
    let color = if kind == JobKind::Backup {
        theme::BLUE
    } else {
        theme::CYAN
    };
    let start = -std::f32::consts::FRAC_PI_2;
    let points = 80usize;
    let count = ((points as f32 * fraction).ceil() as usize).max(1);
    let path: Vec<_> = (0..=count)
        .map(|index| {
            let angle = start + std::f32::consts::TAU * fraction * index as f32 / count as f32;
            center + Vec2::angled(angle) * radius
        })
        .collect();
    ui.painter()
        .add(egui::Shape::line(path, Stroke::new(9.0, color)));
    ui.painter().text(
        center,
        Align2::CENTER_CENTER,
        format!("{:.0}%", fraction * 100.0),
        FontId::proportional(28.0),
        theme::TEXT,
    );
}

fn paint_ambient(ui: &egui::Ui) {
    let rect = ui.max_rect();
    ui.painter().circle_filled(
        rect.right_top() + Vec2::new(-80.0, 10.0),
        260.0,
        theme::BLUE.gamma_multiply(0.035),
    );
    ui.painter().circle_filled(
        rect.left_bottom() + Vec2::new(80.0, -20.0),
        220.0,
        theme::CYAN.gamma_multiply(0.018),
    );
}

fn stage_title(stage: Stage, kind: JobKind) -> &'static str {
    match stage {
        Stage::Preparing => "Preparando la operación…",
        Stage::Copying if kind == JobKind::Backup => "Creando la imagen…",
        Stage::Copying => "Restaurando la imagen…",
        Stage::Verifying => "Verificando el resultado…",
        Stage::Flushing => "Sincronizando el dispositivo…",
    }
}

fn compact_path(path: &Path) -> String {
    let text = path.display().to_string();
    if text.chars().count() <= 74 {
        return text;
    }
    let tail: String = text
        .chars()
        .rev()
        .take(64)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("…{tail}")
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min {:02} s", seconds / 60, seconds % 60)
    } else {
        format!("{} h {:02} min", seconds / 3600, (seconds % 3600) / 60)
    }
}

fn path_is_on_device(path: &Path, device: &DeviceInfo) -> bool {
    let parent = path.parent().unwrap_or(path);
    let normalized_parent = parent
        .canonicalize()
        .unwrap_or_else(|_| parent.to_path_buf());
    device.mount_points.iter().any(|mount| {
        let mount_path = Path::new(mount);
        let normalized_mount = mount_path
            .canonicalize()
            .unwrap_or_else(|_| mount_path.to_path_buf());
        if cfg!(windows) {
            normalized_parent
                .to_string_lossy()
                .to_ascii_lowercase()
                .starts_with(&normalized_mount.to_string_lossy().to_ascii_lowercase())
        } else {
            normalized_parent.starts_with(normalized_mount)
        }
    })
}
