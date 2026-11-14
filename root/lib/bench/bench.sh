# The benchmark suite (`cargo xtask bench`, docs/perf.md). Every result
# is a line `bench NAME ...`: a time in seconds, or `time`'s `Nl Nr Nt`,
# of which xtask takes the real time. The arguments name the rounds
# (by default those in /n/disk/rounds, or 1 2 3); each round runs every
# benchmark once.
#
# /tmp is the kernel's RAM tree and the emu's host directory. /n/disk is
# purgfs on virtio-blk, on the kernel only, when xtask attached a disk
# holding `big`. Styx mounts are an export over a pipe, in the clear and
# encrypted with the test key. mount-sealed-10 and mount-pq-10 time ten
# authenticated mounts each, with the classical and the hybrid
# post-quantum key exchange (docs/auth.md, "Post-quantum options");
# mount-pqsig-10 the same with hybrid Ed25519 + ML-DSA-65 signatures
# both ways (docs/auth.md, "Post-quantum signatures").
load std
rounds=$*
if {~ $#rounds 0 && test -f /n/disk/rounds} {rounds=`{cat /n/disk/rounds}}
if {~ $#rounds 0} {rounds=(1 2 3)}
d=/tmp/bench
rm -rf $d
mkdir -p $d/src $d/clear $d/sealed $d/mfs $d/m
echo bench-setup
# 100 MiB, and 16 MiB for the CPU-bound hash.
zeros 1048576 100 > $d/src/big
zeros 1024 16384 > $d/src/c16
# A signer the exporter trusts, and an authinfo it signed for this user.
mkdir -p $d/keys
auth/createsignerkey -f $d/sk bench > $d/keys/signers
auth/mkauthinfo -f $d/sk `{cat /dev/user} $d/info
auth/createsignerkey -a hybrid -f $d/hsk hbench >> $d/keys/signers
auth/mkauthinfo -f $d/hsk `{cat /dev/user} $d/hinfo
bind $d/keys /lib/keys
mount -A {export -A $d/src} $d/clear
mount -k $d/info {export -e $d/src} $d/sealed
memfs $d/mfs
disk=()
if {test -f /n/disk/big} {disk=/n/disk/big}
# With `kprof` in the kernel's bootargs (`xtask bench --kprof`), profile
# each benchmark below that names itself to kp, and keep the counts on
# the disk for xtask to symbolize. Outside the timed commands.
kprof=()
if {test -f /dev/kpctl && ~ $#disk 1} {
	kprof=/n/disk/kprof
	rm -rf $kprof
	mkdir $kprof
}
fn kp {
	if {~ $#kprof 1} {echo startclr > /dev/kpctl}
}
fn kpsave {
	if {~ $#kprof 1} {
		echo stop > /dev/kpctl
		cat /dev/kpdata > $kprof/$1^.^$r^.pc
		cat /dev/kpcaller > $kprof/$1^.^$r^.lr
	}
}
echo bench-start
for r in $rounds {
	echo bench-round $r
	kp; echo -n 'bench cat-root '; time cat $d/src/big >[2=1] >/dev/null; kpsave cat-root
	kp; echo -n 'bench cat-styx '; time cat $d/clear/big >[2=1] >/dev/null; kpsave cat-styx
	kp; echo -n 'bench cat-styx-sealed '; time cat $d/sealed/big >[2=1] >/dev/null; kpsave cat-styx-sealed
	for f in $disk {
		kp; echo -n 'bench cat-disk '; time cat $f >[2=1] >/dev/null; kpsave cat-disk
	}
	echo -n 'bench sha1-pipe-16m '; time sh -c 'zeros 1024 16384 | sha1sum' >[2=1] >/dev/null
	kp; echo -n 'bench sha1-file-16m '; time sha1sum $d/src/c16 >[2=1] >/dev/null; kpsave sha1-file-16m
	echo -n 'bench sh-builtin-10k '; time sh -c 'load std; ten=(0 1 2 3 4 5 6 7 8 9); for a in $ten {for b in $ten {for c in $ten {for e in $ten {x=$e}}}}' >[2=1] >/dev/null
	echo -n 'bench sh-spawn-1k '; time sh -c 'load std; ten=(0 1 2 3 4 5 6 7 8 9); for a in $ten {for b in $ten {for c in $ten {echo -n}}}' >[2=1] >/dev/null
	echo -n 'bench mount-sealed-10 '; time sh -c 'load std; for i in 0 1 2 3 4 5 6 7 8 9 {mount -k $d/info {export -e $d/src} $d/m; unmount $d/m}' >[2=1] >/dev/null
	echo -n 'bench mount-pq-10 '; time sh -c 'load std; for i in 0 1 2 3 4 5 6 7 8 9 {mount -C x25519mlkem768 -k $d/info {export -C x25519mlkem768 $d/src} $d/m; unmount $d/m}' >[2=1] >/dev/null
	echo -n 'bench mount-pqsig-10 '; time sh -c 'load std; for i in 0 1 2 3 4 5 6 7 8 9 {mount -C mldsa65 -k $d/hinfo {export -C mldsa65 -k $d/hinfo $d/src} $d/m; unmount $d/m}' >[2=1] >/dev/null
	/bin/test/bench.wasm spawn spawn-1k 1000
	/bin/test/bench.wasm fs fs-root-1k $d/src 1000
	/bin/test/bench.wasm fs fs-memfs-1k $d/mfs 1000
	for f in $disk {
		kp; /bin/test/bench.wasm fs fs-disk-1k /n/disk 1000; kpsave fs-disk-1k
	}
}
echo bench-done
unmount $d/mfs
unmount $d/sealed
unmount $d/clear
unmount /lib/keys
rm -rf $d
