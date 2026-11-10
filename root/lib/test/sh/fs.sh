# fs: trees as values. walk, the gates, filter with and without -d,
# select, merge, write, bundle and unbundle, exec, query and eval.
load std
d=/tmp/sh-fs
rm -rf $d
mkdir -p $d/t/src/sub $d/t/obj $d/u/src $d/out
cd $d
echo 'main' > t/src/main.c
echo 'util util' > t/src/util.c
echo 'header' > t/src/sub/x.h
echo 'object' > t/obj/main.o
echo 'read me' > t/README
chmod 444 t/README
chmod 755 t/src/main.c
echo 'other main' > u/src/main.c
echo 'new' > u/src/new.c
echo walk
fs print {walk t}
fs print t/src/sub
echo entries and size
fs print {entries {walk t/obj}}
fs size {walk t}
fs ls {walk t/obj} | sed 's/ .* / /'
fs ls -um {walk t/obj} | sed 's/ .*\[.*\] [0-9]* .* / /'
echo filter
fs print {filter -d {match '*.c'} {walk t}}
fs print {filter {not {match obj}} {walk t}}
fs {filter {or {mode +d} {match -r '^[mR]'}} t}
fs print {filter -d {match -a 't/src/*.c'} t}
echo select
fs print {select {mode +d} {walk t}}
fs print {select {and {mode -d} {mode u+x}} t}
fs print {select {mode a=r} t}
fs print {select {depth 1} t}
fs print {select {path t/src/sub} t}
fs print {select {path -x t/src/sub t/obj} t}
fs print {select {query 'test -d $file'} t}
fs {select '*.h' t}
echo merge
fs print {merge {walk t/src} {walk u/src}}
fs write out/m {merge {walk t/src} {walk u/src}}
cat out/m/main.c out/m/new.c
echo write
fs write out/c {filter {not obj} {walk t}}
fs print {walk out/c}
cmp t/src/sub/x.h out/c/src/sub/x.h && echo same
fs print {select {mode u+x} out/c}
fs print {select {mode -w} out/c}
echo bundle
fs bundle {walk t} > out/b
fs print {unbundle out/b}
fs write out/r {unbundle out/b}
cmp t/obj/main.o out/r/obj/main.o && echo same
fs bundle {filter -d {match '*.h'} t} | fs size {unbundle -}
echo exec and eval
fs exec 'echo got $file' {select {match '*.c'} t}
fs exec -n 2 'wc -c $file' {select {match '*.c'} t}
fs eval 'print {select {match README} t}'
echo errors
fs
fs print
fs walk t/obj
fs mode +d
fs frob t
fs print {walk nosuch}
fs print {filter {walk t} t}
fs print {match '*'}
fs print '{walk t'
fs mode q
fs print {select {mode u+q} t}
echo junk > out/junk
fs print {unbundle out/junk}
fs write out/nodir/x t/README
echo status $status
cd /
rm -rf $d
