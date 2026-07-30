# Script ini dijalankan oleh Magisk Installer, dilarang keras menggunakan shebang (#!/system/bin/sh) di baris pertama!

ui_print "*****************************************"
ui_print "*       ChargerControl (Rust Port)      *"
ui_print "*****************************************"

# Magisk/KernelSU sudah menyediakan variabel $ARCH, $MODPATH, dll
if [ "$ARCH" != "arm64" ]; then
  abort "! Modul ini hanya mendukung arsitektur arm64 (aarch64)."
fi

ui_print "- Arsitektur $ARCH didukung."

DATA_DIR="/data/adb/charger-control"
ui_print "- Menyiapkan konfigurasi di $DATA_DIR..."

mkdir -p "$DATA_DIR"
chmod 700 "$DATA_DIR"

# Buat default config jika belum ada pada saat instalasi
if [ ! -f "$DATA_DIR/config.toml" ]; then
cat > "$DATA_DIR/config.toml" <<EOF
enabled = true
charge_limit = 100
thermal_cutoff = false
max_temp_dc = 400
cpu_power_save = false
poll_interval_secs = 10
log_path = "$DATA_DIR/charger-control.log"
EOF
fi

ui_print "- Instalasi selesai! Silakan reboot."
