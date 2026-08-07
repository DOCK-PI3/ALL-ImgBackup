# ALL ImgBackup

ALL ImgBackup es una aplicación de escritorio nativa para crear copias exactas `.img` de tarjetas SD, memorias USB y discos, y restaurarlas de forma segura. No utiliza tecnologías web: la interfaz y el motor están escritos en Rust y se ejecutan directamente sobre Windows o Linux.

> **Aviso:** restaurar una imagen sobrescribe el dispositivo de destino. ALL ImgBackup bloquea el disco del sistema y exige una confirmación explícita, pero conviene comprobar siempre modelo, capacidad y ruta antes de iniciar.

## Funciones

- Crear imágenes RAW exactas de un dispositivo.
- Restaurar archivos `.img` a un dispositivo físico.
- Verificación SHA-256 opcional después de cada operación.
- Progreso, velocidad, tiempo restante y cancelación.
- Detección nativa de dispositivos en Windows y Linux.
- Identificación y bloqueo del disco que contiene el sistema.
- Bloqueo y desmontaje automático de volúmenes en Windows antes de escribir.
- Impide usar como archivo de imagen el mismo dispositivo que se está procesando.
- Escritura transaccional de backups mediante un archivo `.part`.
- Detección de hardware en segundo plano para que la ventana nunca se bloquee.
- Interfaz oscura nativa, escalable y acelerada por GPU.

## Compilar

Se necesita Rust estable: <https://rustup.rs>

### Windows 10/11

```powershell
cargo build --release
```

El ejecutable queda en `target\release\ALL-ImgBackup.exe`. El binario `release` incluye un manifiesto que solicita permisos de administrador mediante el diálogo UAC de Windows. Las compilaciones de desarrollo no fuerzan la elevación, para que puedan ejecutarse las pruebas automáticas.

El ejecutable de Windows enlaza estáticamente el runtime de Microsoft. Para distribuirlo sólo hace falta `ALL-ImgBackup.exe`; no necesita copiar DLL adicionales ni instalar Visual C++ Redistributable. Las únicas bibliotecas dinámicas que utiliza son componentes incluidos en Windows 10/11.

### Linux

En Debian/Ubuntu, instala las bibliotecas de compilación necesarias:

```bash
sudo apt install build-essential pkg-config libxkbcommon-dev libwayland-dev libxcb-shape0-dev libxcb-xfixes0-dev libssl-dev
cargo build --release
sudo ./target/release/ALL-ImgBackup
```

En Linux, las particiones del destino deben estar desmontadas antes de restaurar. En una instalación final es preferible crear una regla `udev` limitada a dispositivos extraíbles en vez de ejecutar toda la interfaz con `sudo`.

## Seguridad de escritura

- El disco que contiene el sistema operativo aparece marcado y no se puede seleccionar.
- Una restauración exige escribir `BORRAR` después de revisar modelo, capacidad y ruta física.
- Windows bloquea y desmonta las letras de unidad asociadas al disco durante la operación.
- Linux rechaza una restauración mientras alguna partición del destino siga montada.
- Una imagen no puede almacenarse ni leerse desde el mismo dispositivo que actúa como origen/destino.
- Al crear un backup se escribe primero `archivo.img.part`; el nombre final sólo aparece después de completar la copia y, si está activada, su verificación.

## Desarrollo

```bash
cargo run
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

## Formato

Las imágenes son RAW, sector por sector, compatibles con Win32 Disk Imager, Raspberry Pi Imager, `dd` y herramientas equivalentes. La primera versión no comprime las imágenes.

## Estructura

- `src/app.rs`: experiencia de usuario y estados de operación.
- `src/devices/`: detección de discos específica de Windows y Linux.
- `src/imaging.rs`: copia por bloques, SHA-256, cancelación y sincronización.
- `assets/`: manifiestos DPI/UAC para Windows.
