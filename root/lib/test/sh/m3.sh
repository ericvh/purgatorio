# memfs, ramfile, and a namespace line that names the mount.
mkdir -p /tmp/m3d/fs
memfs /tmp/m3d/fs
echo hello > /tmp/m3d/fs/a
cat /tmp/m3d/fs/a
ramfile /tmp/m3d/ram
echo xyz > /tmp/m3d/ram
cat /tmp/m3d/ram
ns | grep -c /tmp/m3d/fs
ps | grep -c memfs
ps | grep -c ramfile
