# fs: the modules fs.sh leaves out. compose, merge -1 and -c, exec -p,
# -P and -t, @{...} blocks, pipe, run, setroot, proto, chstat,
# mergewrite, void and write to the tree's own root.
d=/tmp/sh-fsmods
rm -rf $d
mkdir -p $d/t/a $d/u/a $d/out
cd $d
echo one > t/a/x
echo two > t/b
echo three > u/a/x
echo four > u/a/y
echo five > u/c
chmod 644 t/a/x t/b u/a/x u/a/y u/c
chmod 755 t t/a u u/a
echo compose and merge
fs print {merge -c {compose AinB} t u}
fs print {merge -c {compose AoutB} t u}
fs print {merge -c {compose -d AxorB} t u}
fs print {merge -1 t u}
fs print {merge -1 -c {compose BoverA} t u}
fs pipe -1 cat {merge -c {compose BoverA} t u}
fs print {merge -c {compose AatopB} t u t}
echo exec
fs exec @{echo got $file} {select {mode -d} t}
fs exec -p @{echo $file $mode $length} {select {mode -d} t}
fs exec -P @{echo $name $#qid $#dtype} {select {mode -d} t}
fs exec -t @{echo last $n $#file} @{n=$file} t
fs exec -n 2 -t 'echo $#all' 'all=($all $file)' t
echo pipe
fs pipe @{echo $file: ; cat} t
fs pipe -p 'echo $length; wc -c' t
fs pipe -1 'wc -c' t
fs pipe read u
echo run
fs print {walk @{s=t/a}}
fs {setroot @{s=new} t}
echo setroot
fs print {setroot new t}
fs print {setroot -c /p/q t}
fs print {setroot -c p t/a}
cd out
fs write '' {setroot -c deep/er ../t}
cd ..
fs print out/deep
cat out/deep/er/a/x
echo proto
echo 'a
	x
b
c	- - -	/a/x' > proto
fs print {proto -r t proto}
fs write out/p {setroot p {proto -r t proto}}
cat out/p/c
echo '+' > all
fs print {proto -r u all}
echo chstat
fs ls {chstat -m go-r {walk t}} | sed 's/ .*//'
fs ls {chstat -c -m u-w {walk t}} | sed 's/ .*//'
fs ls {chstat -p {mode -d} -m a+x t} | sed 's/ .*//'
fs exec -p 'echo $uid $mtime' {chstat -u bob -t 1000 t}
echo mergewrite
cp -r t m
fs mergewrite m u
fs print m
cat m/a/x m/b
fs mergewrite -c {compose AoutB} m t
fs print m
fs mergewrite -c {compose -d AxorB} m u
fs print m
fs mergewrite -c {compose B} m t
fs print m
cat m/a/x
echo void
fs void t
fs void {print t}
echo errors
fs compose AunderB
fs print {merge -c {match x} t u}
fs exec -n 2 -p 'echo' t
fs pipe -1 -p cat t
fs run @{s=(a b)}
fs run @{s=()}
fs setroot '' t
fs chstat -m q t
fs chstat -t never t
fs chstat -z t
fs merge -c
fs print {proto nosuch}
echo status $status
cd /
rm -rf $d
