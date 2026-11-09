# Background jobs, wait, run, and $apid.
d=/tmp/sh-jobs
rm -rf $d
mkdir $d
{ sleep 3; echo late > $d/late } &
echo started
ls $d
wait
cat $d/late
echo 'echo ran with $*' > $d/script
run $d/script x y
sleep 0.1 &
echo $#apid
wait
sh -c 'exit three'
echo $status
rm -r $d
