# Reparador de Videos Corruptos

App de escritorio para **Linux** (Tauri v2 + Rust + Tailwind, sin framework JS) que repara videos que llegan mal-etiquetados como "documento" en redes/mensajería — los remuxea (`ffmpeg -c copy`, sin recodificar) a un MP4/contenedor real y reproducible.

Pensada para correr **en segundo plano de forma optimizada**, no solo como ventana que abrís y cerrás:

- **Cola de reparación**: elegí o arrastrá varios videos a la vez, se procesan uno tras otro, con carpeta de destino elegible.
- **Bandeja del sistema**: cerrar la ventana la esconde en vez de matar el proceso. Se sigue usando desde el ícono de bandeja (Mostrar/Salir).
- **Carpeta vigilada**: elegís una carpeta y todo lo que caiga ahí se repara solo, sin necesidad de tener la ventana abierta — el resultado queda en una subcarpeta `reparados/` adentro.
- **Integración con Dolphin (KDE)**: clic derecho sobre cualquier archivo → "Reparar con Reparador de Videos", lo abre ya cargado en la cola (usa instancia única: si la app ya está corriendo, no abre una ventana nueva).

## Requisitos

- Linux con `ffmpeg` y `ffprobe` instalados y en el `PATH`.
- Para compilar: Rust (`cargo`) y Node.js/npm.

## Compilar

```sh
cd gui
npm install
npm run build:css
cargo build --release --manifest-path src-tauri/Cargo.toml
```

El binario queda en `src-tauri/target/release/gui`.

## Integración opcional con Dolphin

```sh
mkdir -p ~/.local/share/kio/servicemenus
cp packaging/reparador-videos-servicemenu.desktop ~/.local/share/kio/servicemenus/reparador-videos.desktop
```

Editá el `Exec=`/`Icon=` de ese archivo para que apunten a donde tengas el binario compilado.
