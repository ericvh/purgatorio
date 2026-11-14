# The hybrid post-quantum key exchange (docs/auth.md, "Post-quantum
# options"). mount -C x25519mlkem768 takes it from any export, through
# the kernel's own mount and through fauth (-S with a dial string); an
# export with -C x25519mlkem768 serves only that. A classical mount of a
# plain export still works, and of a post-quantum-only export is refused.
# A server that asks for no proof cannot give the exchange.
load std
bind /lib/test/keys /lib/keys
d=/tmp/sh-pqmount
rm -rf $d
mkdir -p $d/n
styxlisten tcp!127.0.0.1!0 export / >[2] $d/el &
epid=$apid
styxlisten tcp!127.0.0.1!0 export -C x25519mlkem768 / >[2] $d/ql &
qpid=$apid
styxlisten tcp!127.0.0.1!0 export -A / >[2] $d/al &
apid2=$apid
for f in el ql al {while {! test -s $d/$f} {sleep 0}}
e=tcp!`{sed 's/styxlisten: //' $d/el}
q=tcp!`{sed 's/styxlisten: //' $d/ql}
a=tcp!`{sed 's/styxlisten: //' $d/al}
mount -C x25519mlkem768 $e $d/n && wc -c < $d/n/lib/test/auth.key.pub && unmount $d/n
mount $e $d/n && echo classical && unmount $d/n
mount -C x25519 $e $d/n && echo named classical && unmount $d/n
mount -C x25519mlkem768 $q $d/n && echo pq only && unmount $d/n
mount -S -C x25519mlkem768 $q $d/n && echo pq and server && unmount $d/n
import -C x25519mlkem768 $q $d/n && echo import && unmount $d/n
mount $q $d/n
mount -S $q $d/n
mount -C x25519mlkem768 $a $d/n
mount -A -C x25519mlkem768 $e $d/n
mount -C rc4_256 $e $d/n
kill $epid $qpid $apid2
rm -rf $d
