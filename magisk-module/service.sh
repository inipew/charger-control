#!/system/bin/sh
# $MODDIR otomatis disetel oleh Magisk/KernelSU ke path modul saat ini
# Dilarang menggunakan MODDIR="${0%/*}" karena bisa rusak/berbeda di lingkungan Magisk

# Tunggu Android selesai booting sepenuhnya sebelum menjalankan daemon baterai
while [ "$(getprop sys.boot_completed)" != "1" ]; do
    sleep 2
done

# Beri permission ke file sysfs
${MODDIR}/system/bin/charger-ctl grant-perms

# Eksekusi background daemon (gunakan path absolut ke $MODDIR)
nohup ${MODDIR}/system/bin/charger-daemon > /dev/null 2>&1 &
