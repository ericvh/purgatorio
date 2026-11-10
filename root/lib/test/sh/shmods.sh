# load arg, echo, csv and test: the sh-arg(1), echo(1), sh-csv(1) and
# sh-test(1) builtins.
load std arg echo csv
d=/tmp/shmods
rm -rf $d
mkdir -p $d

# arg: the example from sh-arg(1), with if for its $opt^flag.
fn script {
	bflag=0
	cflag=0
	file=()
	rect=()
	args=$*
	(arg
		bc {if {~ $opt b} {bflag=1} {cflag=1}}
		f+ {file=$arg}
		r++++ {rect=$arg}
		'*' {echo unknown option $opt}
		- $args
	)
	echo $bflag $cflag $file
	echo rect $rect
	echo arguments are $*
}
script -bc -r 0 10 50 100 -ffile a b c
script -b -f file -z -- -bc
# Without -, arg parses $*; - alone and a non-option end the options.
fn noargs {arg x {echo x}; echo rest $*}
noargs -x - -x
noargs -xx y -x
# A missing argument or an unknown option raises usage.
{arg f+ {} - -f} >[2=1] | cat
echo status $status
{arg a {} - -b} >[2=1] | cat
echo status $status
{arg a} >[2=1] | cat
# break stops the parsing and leaves $* alone.
fn brk {arg a {echo a} b {raise break} - -a -b -a; echo after $*}
brk keep

# echo, now a builtin.
echo -n no newline
echo
echo -- -n
echo a b > $d/e
cat $d/e
echo out >[1=]
echo status $status

# csv: ${csv} quotes as needed and getcsv splits lines into $line.
echo ${csv a 'b,c' 'say "hi"' ''}
echo ${csv}
echo ${csv one}
echo 'a,b,c' > $d/in
echo '"x,y",' >> $d/in
echo '' >> $d/in
echo '"multi' >> $d/in
echo 'line",z' >> $d/in
echo 'skip,me' >> $d/in
echo 'stop,here' >> $d/in
echo never >> $d/in
getcsv {
	if {~ ${hd $line} skip} {raise continue}
	if {~ ${hd $line} stop} {raise break}
	echo $#line items: ${quote $line}
} < $d/in
touch $d/empty
getcsv {echo $line} < $d/empty
echo status $status

# test: report goes nowhere when run standalone, and to $ITS_MFD under
# itest, as severity, verbosity and message.
{load test; report INF 5 quiet} >[2=1] | cat
ITS_VERBOSITY=5
ITS_MFD=1
{load test; report ERR 0 'cmp failed'; report INF 6 ok fine} | grep -v '^40'
{load test; report BAD 0 x} >[2=1] | grep -v '^40'
ITS_VERBOSITY=()
ITS_MFD=()
rm -rf $d
