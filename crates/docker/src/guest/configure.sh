#!/bin/sh
# Writes vat-owned configuration into the real root on every boot, so a newer
# vat can change it without re-provisioning.
set -eu
R=$1
G=/mnt/vat/guest
cp "$G/inittab" "$R/etc/inittab"
echo vat > "$R/etc/hostname"
printf '127.0.0.1 localhost vat\n::1 localhost\n' > "$R/etc/hosts"
if [ -f "$G/hosts.extra" ]; then cat "$G/hosts.extra" >> "$R/etc/hosts"; fi
mkdir -p "$R/etc/docker" "$R/etc/chrony" "$R/usr/share/udhcpc" "$R/etc/sysctl.d"
cp "$G/daemon.json" "$R/etc/docker/daemon.json"
cp "$G/udhcpc.script" "$R/usr/share/udhcpc/vat.script"
chmod 755 "$R/usr/share/udhcpc/vat.script"
printf 'pool pool.ntp.org iburst\nmakestep 1 -1\nrtcsync\n' > "$R/etc/chrony/chrony.conf"
# Later `apk add` inside the guest uses the mirror the host picked.
if [ -s "$G/alpine.mirror" ]; then
  . "$G/versions.env"
  m=$(cat "$G/alpine.mirror")
  printf '%s/%s/main\n%s/%s/community\n' "$m" "$ALPINE_BRANCH" "$m" "$ALPINE_BRANCH" > "$R/etc/apk/repositories"
fi
sed -i 's/^#*rc_cgroup_mode=.*/rc_cgroup_mode="unified"/' "$R/etc/rc.conf"
# binfmt_misc holds the Rosetta binary open ('F' flag), so unmounting it at
# shutdown only burns retries.
mkdir -p "$R/etc/conf.d"
printf 'no_umounts="/mnt/rosetta"\n' > "$R/etc/conf.d/localmount"
mkdir -p "$R/usr/local/bin"
cp "$G/vat-guest" "$R/usr/local/bin/vat-guest.new"
chmod 755 "$R/usr/local/bin/vat-guest.new"
mv "$R/usr/local/bin/vat-guest.new" "$R/usr/local/bin/vat-guest"
for svc in vat-net vat-agent k3s; do
  cp "$G/$svc.initd" "$R/etc/init.d/$svc"
  chmod 755 "$R/etc/init.d/$svc"
done
# The system bundle is rebuilt at boot (vat-net) when the CA changed: the
# initramfs chroot cannot run update-ca-certificates reliably.
C="$R/usr/local/share/ca-certificates/vat-ca.crt"
if [ -f "$G/ca.pem" ]; then
  mkdir -p "$R/usr/local/share/ca-certificates"
  if ! cmp -s "$G/ca.pem" "$C"; then
    cp "$G/ca.pem" "$C"
    : > "$R/etc/ssl/.vat-ca-stale"
  fi
  # dockerd trusts the local Artifact Registry through its per-registry CA dir.
  if [ -f "$G/registry.hosts" ]; then
    while read -r h; do
      [ -n "$h" ] || continue
      mkdir -p "$R/etc/docker/certs.d/$h"
      cp "$G/ca.pem" "$R/etc/docker/certs.d/$h/ca.crt"
    done < "$G/registry.hosts"
  fi
elif [ -f "$C" ]; then
  rm -f "$C"
  : > "$R/etc/ssl/.vat-ca-stale"
fi
link() { mkdir -p "$R/etc/runlevels/$1"; ln -sf "/etc/init.d/$2" "$R/etc/runlevels/$1/$2"; }
link sysinit devfs
link sysinit dmesg
link boot hostname
link boot sysctl
link boot bootmisc
link boot cgroups
link boot vat-net
link default chronyd
link default docker
link default vat-agent
if [ -f "$G/k3s.enabled" ]; then
  # The host stages the pinned K3s binary (it bundles cri-dockerd for
  # `--docker`); only machines with K8s enabled pay for it.
  . "$G/versions.env"
  have=$("$R/usr/local/bin/k3s" --version 2>/dev/null | head -n1 | cut -d' ' -f3 || true)
  if [ "$have" != "$K3S_VERSION" ]; then
    echo "configure: installing k3s $K3S_VERSION"
    cp "$G/k3s" "$R/usr/local/bin/k3s.part"
    chmod 755 "$R/usr/local/bin/k3s.part"
    mv "$R/usr/local/bin/k3s.part" "$R/usr/local/bin/k3s"
  fi
  link default k3s
  # K3s applies (and on removal deletes) everything in its manifests dir.
  M="$R/var/lib/rancher/k3s/server/manifests"
  mkdir -p "$M"
  if [ -f "$G/k3s-vat-gcp.yaml" ]; then
    cp "$G/k3s-vat-gcp.yaml" "$M/vat-gcp.yaml"
  else
    rm -f "$M/vat-gcp.yaml"
  fi
else
  rm -f "$R/etc/runlevels/default/k3s"
fi
printf '%s\n' \
  'fs.inotify.max_user_watches=1048576' \
  'fs.inotify.max_user_instances=8192' \
  'net.ipv4.ip_forward=1' \
  'vm.max_map_count=262144' > "$R/etc/sysctl.d/90-vat.conf"
