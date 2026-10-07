#!/bin/sh
# Installs the guest root filesystem into $1. Idempotent: re-running on an
# existing root upgrades/adds packages. Runs inside the initramfs.
set -eu
R=$1
G=/mnt/vat/guest
. "$G/versions.env"
MIRROR=$(cat "$G/alpine.mirror" 2>/dev/null || echo https://dl-cdn.alpinelinux.org/alpine)
REPO=$MIRROR/$ALPINE_BRANCH
mkdir -p "$R/etc/apk" "$R/dev" "$R/proc" "$R/sys"
cp -r /etc/apk/keys "$R/etc/apk/"
printf '%s/main\n%s/community\n' "$REPO" "$REPO" > "$R/etc/apk/repositories"
mount --bind /dev "$R/dev"
mount --bind /proc "$R/proc"
mount --bind /sys "$R/sys"
trap 'umount "$R/dev" "$R/proc" "$R/sys" 2>/dev/null' EXIT
echo "provision: apk start $(date +%T)"
apk --root "$R" --initdb --update-cache --no-progress add \
  alpine-base openrc busybox-openrc busybox-mdev-openrc \
  docker docker-openrc docker-cli-buildx iptables ip6tables chrony \
  e2fsprogs ca-certificates curl findutils util-linux-misc

echo "provision: apk done $(date +%T)"
