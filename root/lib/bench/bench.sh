# The benchmark suite (`cargo xtask bench`, docs/perf.md). Every result
# is a line `bench NAME ...`: a time in seconds, or `time`'s `Nl Nr Nt`,
# of which xtask takes the real time. The arguments name the rounds
# (by default those in /n/disk/rounds, or 1 2 3); each round runs every
# benchmark once.
#
# /tmp is the kernel's RAM tree and the emu's host directory. /n/disk is
# purgfs on virtio-blk, on the kernel only, when xtask attached a disk
# holding `big`. Styx mounts are an export over a pipe, in the clear and
# encrypted with the test key.
load std
rounds=$*
if {~ $#rounds 0 && test -f /n/disk/rounds} {rounds=`{cat /n/disk/rounds}}
if {~ $#rounds 0} {rounds=(1 2 3)}
d=/tmp/bench
rm -rf $d
mkdir -p $d/src $d/clear $d/sealed $d/mfs
echo bench-setup
# 100 MiB, and 16 MiB for the CPU-bound hash.
zeros 1048576 100 > $d/src/big
zeros 1024 16384 > $d/src/c16
# A signer the exporter trusts, and an authinfo it signed for this user.
mkdir -p $d/keys
auth/createsignerkey -f $d/sk bench > $d/keys/signers
auth/mkauthinfo -f $d/sk `{cat /dev/user} $d/info
bind $d/keys /lib/keys
mount -A {export -A $d/src} $d/clear
mount -k $d/info {export -e $d/src} $d/sealed
memfs $d/mfs
disk=()
if {test -f /n/disk/big} {disk=/n/disk/big}
echo bench-start
for r in $rounds {
	echo bench-round $r
	echo -n 'bench cat-root '; time cat $d/src/big >[2=1] >/dev/null
	echo -n 'bench cat-styx '; time cat $d/clear/big >[2=1] >/dev/null
	echo -n 'bench cat-styx-sealed '; time cat $d/sealed/big >[2=1] >/dev/null
	for f in $disk {
		echo -n 'bench cat-disk '; time cat $f >[2=1] >/dev/null
	}
	echo -n 'bench sha1-pipe-16m '; time sh -c 'zeros 1024 16384 | sha1sum' >[2=1] >/dev/null
	echo -n 'bench sha1-file-16m '; time sha1sum $d/src/c16 >[2=1] >/dev/null
	echo -n 'bench sh-builtin-10k '; time sh -c 'load std; ten=(0 1 2 3 4 5 6 7 8 9); for a in $ten {for b in $ten {for c in $ten {for e in $ten {x=$e}}}}' >[2=1] >/dev/null
	echo -n 'bench sh-spawn-1k '; time sh -c 'load std; ten=(0 1 2 3 4 5 6 7 8 9); for a in $ten {for b in $ten {for c in $ten {echo -n}}}' >[2=1] >/dev/null
	/bin/test/bench.wasm spawn spawn-1k 1000
	/bin/test/bench.wasm fs fs-root-1k $d/src 1000
	/bin/test/bench.wasm fs fs-memfs-1k $d/mfs 1000
	for f in $disk {
		/bin/test/bench.wasm fs fs-disk-1k /n/disk 1000
	}
}
echo bench-done
unmount $d/mfs
unmount $d/sealed
unmount $d/clear
unmount /lib/keys
rm -rf $d
