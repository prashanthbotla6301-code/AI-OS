#!/bin/bash
cd /home/user/AI-OS
export PATH=/opt/aosp-tools/bin:$PATH LD_LIBRARY_PATH=/opt/aosp-tools/lib
export BISON_PKGDATADIR=/opt/aosp-tools/share/bison M4=/opt/aosp-tools/bin/m4
SRC=build/kernel/linux-6.12.104
KOUT=build/kernel/build-6.12.104
complete=0
for i in 1 2 3 4 5 6; do
    echo "=== clone attempt $i $(date)"
    if [ -d "$SRC/.git" ]; then
        if git -C "$SRC" checkout -f HEAD -- . 2>/dev/null && [ -z "$(git -C "$SRC" status --porcelain 2>/dev/null)" ]; then
            complete=1; break
        fi
        rm -rf "$SRC"
    fi
    git clone --filter=blob:none --depth 1 --branch v6.12.104 --single-branch https://github.com/gregkh/linux.git "$SRC" > /tmp/kclone.log 2>&1
    if [ -d "$SRC/.git" ] && git -C "$SRC" checkout -f HEAD -- . > /dev/null 2>&1 && [ -z "$(git -C "$SRC" status --porcelain 2>/dev/null)" ]; then
        complete=1; break
    fi
    sleep 5
done
[ "$complete" = 1 ] || { echo "KERNEL BUILD FAILED: no complete checkout"; exit 1; }
echo "=== source tree complete"
rm -rf "$KOUT"
mkdir -p "$KOUT"
make -C "$SRC" O="$PWD/$KOUT" ARCH=x86_64 defconfig || { echo "KERNEL BUILD FAILED: defconfig"; exit 1; }
(cd "$KOUT" && "$PWD/$SRC/scripts/kconfig/merge_config.sh" -m .config "$PWD/kernel/config-x86_64" > /dev/null) || { echo "KERNEL BUILD FAILED: merge"; exit 1; }
# sandbox accommodation: no OpenSSL available in this environment (crypto
# sources are egress-blocked), so drop the built-in keyring certs tooling
# (unused by AgentOS: no modules, no kexec, no signed-kernel loading).
sed -i -e 's/^CONFIG_SYSTEM_TRUSTED_KEYRING=.*/# CONFIG_SYSTEM_TRUSTED_KEYRING is not set/' \
       -e 's/^CONFIG_SYSTEM_BLACKLIST_KEYRING=.*/# CONFIG_SYSTEM_BLACKLIST_KEYRING is not set/' \
       -e 's/^CONFIG_SYSTEM_REVOCATION_LIST=.*/# CONFIG_SYSTEM_REVOCATION_LIST is not set/' "$KOUT/.config"
make -C "$SRC" O="$PWD/$KOUT" ARCH=x86_64 olddefconfig || { echo "KERNEL BUILD FAILED: olddefconfig"; exit 1; }
if ./scripts/build-kernel.sh; then
    echo "KERNEL BUILD OK"
    exit 0
fi
echo "KERNEL BUILD FAILED: make"
exit 1
