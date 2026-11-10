# Styx tools: styxchat, styxmon, iostats, nsbuild and fcp.
d=/tmp/sh-styxtools
rm -rf $d
mkdir -p $d/n $d/src/sub
cd $d
# styxchat as a client of memfs, with the data of reads and writes.
echo 'Tversion 8192 9P2000
Tattach 0 nofid inferno ''''
Tcreate 0 a 8r644 2
Twrite 0 0 ''hello there''
Tclunk 0
# a comment
Tattach 1 nofid inferno ''''
Twalk 1 2 a
Topen 2 0
Tread 2 6 100
Tstat 2
Twstat 2 b '''' '''' ~0 ~0 ~0
Twalk 1 3 nope
Tflush 99
Topen 1
Tbogus
nexttag
nexttag 20
Tclunk 2' > cmds
styxchat -v {memfs --serve} < cmds
# styxchat as a server: mount's version and attach, answered by hand.
echo 'Rversion 65535 8192 9P2000
Rattach 0 5.0.d' > replies
styxchat -s {mount -A 0 n} < replies | sed 's/Attach\(0,1,4294967295,"[^"]*"/Attach(0,1,4294967295,"user"/'
unmount n
# styxmon between styxchat and memfs; messages on its standard error.
echo 'Tversion 8192 9P2000
Tattach 0 nofid inferno ''''
Tcreate 0 x 8r600 1
Twrite 0 0 data
Tclunk 0' > cmds
styxchat {styxmon -d memfs --serve} < cmds >[2=1]
# styxmon under a mount.
mount -c {styxmon memfs --serve >[2] $d/mon.log} n
echo hi > n/x
cat n/x
unmount n
grep -c Tmsg.Create mon.log
grep 'Rmsg.Read.*array\[3\]' mon.log
# iostats: the files a command used, and its exit status.
echo hello > f
iostats cat f >[2] stats
grep ' /tmp/sh-styxtools/f$' stats
grep -c '^Tread ' stats
iostats -d -f dbg sh -c 'echo x > g; exit bad' >[2] /dev/null
echo $status
cat g
grep -c '^->Tmsg.Create' dbg
iostats >[2=1]
# nsbuild: binds, includes, variables and errors.
mkdir -p a b c
echo in a > a/f
echo in b > b/f
echo 'bind -b a b' > inc
echo '# a namespace
bind $d/a $d/c
. inc
bind $nosuch $d/c
unmount -i $d/x' > namespace
nsbuild
cat b/f c/f
echo 'bind -z a b' > bad
nsbuild bad >[2=1]
echo 'frob x' > bad
nsbuild bad >[2=1]
echo 'bind missing c' > bad
nsbuild bad >[2=1]
echo 'cd a b' > bad
nsbuild bad >[2=1]
nsbuild nope >[2=1]
unmount b
unmount c
# fcp: files, trees and errors.
echo one > src/1
echo two > src/sub/2
chmod 600 src/1
fcp -R 2 -W 3 src/1 copy
cmp src/1 copy && echo copied
ls -l copy | sed 's/ .*//'
mkdir dst
fcp -r src dst
cat dst/src/1 dst/src/sub/2
fcp -r src newtree
ls newtree newtree/sub
fcp src x >[2=1]
fcp src/1 src/sub/2 copy >[2=1]
fcp src/1 src/1 >[2=1]
fcp -R 0 src/1 x >[2=1]
cd /
rm -r $d
