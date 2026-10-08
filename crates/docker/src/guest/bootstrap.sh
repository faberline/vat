#!/bin/sh
# Runs as PID 1 inside the initramfs, read from the state share on every boot.
# Brings up networking, formats/mounts the persistent data disk, provisions
# the real root on first boot (or when the provision version changes), wires
# host shares, and switch_roots into OpenRC.
set -u
STATE=/mnt/vat
G=$STATE/guest
log() { echo "vat-bootstrap: $*"; }
phase() { printf '{"phase":"%s","detail":"%s","at":%s}\n' "$1" "${2:-}" "$(date +%s)" > "$STATE/boot.json"; }
fail() {
  log "FAILED: $*"
  phase failed "$*"
  sleep 1
  poweroff -f
}

for arg in $(cat /proc/cmdline); do
  case "$arg" in
    vat.time=*) date -s "@${arg#vat.time=}" >/dev/null 2>&1 ;;
  esac
done
phase booting

ip link set lo up
ip link set eth0 up
cp "$G/udhcpc.script" /etc/udhcpc.script
chmod 755 /etc/udhcpc.script
udhcpc -i eth0 -q -n -t 20 -T 1 -s /etc/udhcpc.script >/dev/null 2>&1 || log "dhcp failed; continuing"

# ext4 superblock magic 0xEF53 lives at byte 1080 of the device.
if ! dd if=/dev/vda bs=1 skip=1080 count=2 2>/dev/null | od -An -tx1 | grep -q '53 ef'; then
  phase formatting
  log "formatting the data disk"
  if [ -s "$G/alpine.mirror" ]; then
    m=$(cat "$G/alpine.mirror")
    printf '%s/v3.22/main\n%s/v3.22/community\n' "$m" "$m" > /etc/apk/repositories
  fi
  apk add --no-cache --quiet e2fsprogs || fail "cannot install e2fsprogs"
  mkfs.ext4 -q -F -L vatdata -E lazy_itable_init=1,lazy_journal_init=1 /dev/vda || fail "mkfs.ext4 failed"
fi
mkdir -p /newroot
mount -t ext4 -o noatime /dev/vda /newroot || fail "cannot mount the data disk"

want=$(cat "$G/version")
have=$(cat /newroot/etc/vat-provisioned 2>/dev/null || true)
if [ "$want" != "$have" ]; then
  phase provisioning "$want"
  log "provisioning root ($have -> $want)"
  sh "$G/provision.sh" /newroot || fail "provisioning failed"
  echo "$want" > /newroot/etc/vat-provisioned
fi
sh "$G/configure.sh" /newroot || fail "configuration failed"

# Host shares: one virtiofs device with several named directories, each bound
# at the same absolute path it has on the host so bind mounts just work.
mkdir -p /newroot/mnt/host
if mount -t virtiofs vat-host /newroot/mnt/host; then
  while read -r name target; do
    [ -n "$name" ] || continue
    [ -d "/newroot/mnt/host/$name" ] || continue
    mkdir -p "/newroot$target"
    mount --bind "/newroot/mnt/host/$name" "/newroot$target"
  done < "$G/host-mounts"
else
  log "host share unavailable"
fi

# Rosetta: register x86_64 ELF with the 'F' flag so the interpreter is opened
# now and stays valid across switch_root.
mkdir -p /newroot/mnt/rosetta
if mount -t virtiofs rosetta /newroot/mnt/rosetta 2>/dev/null; then
  mount -t binfmt_misc binfmt_misc /proc/sys/fs/binfmt_misc 2>/dev/null
  if [ ! -e /proc/sys/fs/binfmt_misc/rosetta ]; then
    printf '%s' ':rosetta:M::\x7fELF\x02\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00\x02\x00\x3e\x00:\xff\xff\xff\xff\xff\xfe\xfe\x00\xff\xff\xff\xff\xff\xff\xff\xff\xfe\xff\xff\xff:/newroot/mnt/rosetta/rosetta:CF' \
      > /proc/sys/fs/binfmt_misc/register || log "rosetta binfmt registration failed"
  fi
fi

if [ -s /etc/resolv.conf ]; then
  cp /etc/resolv.conf /newroot/etc/resolv.conf
fi
# The state share is about to move under the new root; record the last
# initramfs phase while it is still at $STATE.
phase starting
mkdir -p /newroot/mnt/vat
mount --move "$STATE" /newroot/mnt/vat
mount --move /dev /newroot/dev
mount --move /sys /newroot/sys
mount --move /proc /newroot/proc
exec switch_root /newroot /sbin/init
