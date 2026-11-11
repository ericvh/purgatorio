# The Lethe client, #λ, with no server (docs/lethe.md): its clone file,
# settings it refuses (noauth, the clear mode, takes no key and no user),
# and the lethe command's errors. Reading a tree
# needs lethed on the host: crates/purg-emu/tests/lethe.rs, and the
# kernel's network checks in cargo xtask kernel.
mkdir -p /tmp/sh-lethe
ls '#λ'
ls -d '#λ/clone'
echo nonsense > '#λ/clone'
echo 'dial udp!127.0.0.1!1' > '#λ/clone'
read < '#λ/clone'
echo 'dial tcp!127.0.0.1!1
noauth
key 00' > '#λ/clone'
echo 'dial tcp!127.0.0.1!1
noauth yes' > '#λ/clone'
echo 'dial tcp!127.0.0.1!1
noauth' > '#λ/clone'
ls '#λ99'
lethe
lethe -k /nonexistent tcp!127.0.0.1!1 /tmp/sh-lethe
lethe tcp!127.0.0.1!1 /tmp/sh-lethe
lethe -A -k /nonexistent tcp!127.0.0.1!1 /tmp/sh-lethe
lethe -A -u glenda tcp!127.0.0.1!1 /tmp/sh-lethe
lethe -A tcp!127.0.0.1!1 /tmp/sh-lethe
rm -r /tmp/sh-lethe
