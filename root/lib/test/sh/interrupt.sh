# The interrupt note (docs/abi.md, "Interrupts"): an interactive sh takes a
# process group of its own and survives the note; the command it was
# running dies of it, the rest of that line is abandoned, and sh reads on.
load std
d=/tmp/sh-interrupt
rm -rf $d
mkdir $d
echo 'sleep 30; echo not reached
echo after' | sh -i > $d/out >[2=1] &
sh=$apid
while {! ps | grep -s ' sleep$'} {sleep 0.05}
echo interruptgrp > /prog/$sh/ctl
wait
cat $d/out
echo
# Without catchint, the note kills.
sleep 30 &
echo interrupt > /prog/$apid/ctl
wait
ps | grep -s ' sleep$' || echo no sleep left
rm -r $d
