# syntax=docker/dockerfile:1.5

FROM alpine:edge

SHELL ["/bin/sh", "-ce"]

ENV WINEPREFIX="/prefix"
ENV WINEARCH="win64"
ENV WINEDEBUG="-all"

RUN <<EOF_RUN

echo "https://cdn.alpinelinux.org/edge/testing" >> /etc/apk/repositories
apk update
apk add --no-cache tar wine wine-mono
apk cache clean

wineboot -i
while pgrep wineserver > /dev/null; do sleep 1; done

# Set up case-sensitive symlinks
ln -s /prefix/drive_c/users /prefix/drive_c/Users
ln -s /prefix/drive_c/users /prefix/drive_c/USERS
ln -s /prefix/drive_c/windows /prefix/drive_c/Windows
ln -s /prefix/drive_c/windows /prefix/drive_c/WINDOWS
ln -s /prefix/drive_c/windows/system32 /prefix/drive_c/windows/System32
ln -s /prefix/drive_c/windows/system32 /prefix/drive_c/windows/SYSTEM32
ln -s /prefix/drive_c/windows/syswow64 /prefix/drive_c/windows/SysWOW64
ln -s /prefix/drive_c/windows/syswow64 /prefix/drive_c/windows/SYSWOW64
ln -s /prefix/drive_c/windows/winsxs /prefix/drive_c/windows/WinSxS
ln -s /prefix/drive_c/windows/winsxs /prefix/drive_c/windows/WINSXS

EOF_RUN
