# stream, timestamp and pause.
d=/tmp/streamtest
rm -rf $d
mkdir -p $d
echo hello stream > $d/s1
stream $d/s1
stream -b 2 $d/s1
stream -a $d/s1
sleep 1
stream $d/s1 /dev/null
echo $status
cat $d/s1
stream $d/nosuch
echo $status
stream -b 0 $d/s1
stream

# timestamp: the times vary, so only the rest of each line is kept.
{echo one; echo two} | timestamp eg | sed 's/^[0-9]+ //' | sed 's/start [0-9]+$/start/'
echo plain | timestamp | sed 's/^[0-9]+ //' | sed 's/start [0-9]+$/start/'
echo plain | timestamp | sed 's/ .*//' | sed 's/[0-9]/d/g'

# pause never returns, until it is killed.
pause &
p=$apid
sleep 0.2
kill $p
wait
echo paused
rm -rf $d
