# Archives: tar, gettar, puttar, lstar, gzip and gunzip.
d=/tmp/sh-archive
f=/lib/test/archive
rm -rf $d
mkdir -p $d/src/sub/deep $d/out $d/get
echo hello > $d/src/a
echo world > $d/src/sub/b
echo deep > $d/src/sub/deep/c
cd $d
# gzip and gunzip as filters, and their fixed header.
echo hello hello hello | gzip | gunzip
echo hello | gzip | xd
echo hello | gzip -9 | gunzip
# Streams GNU gzip wrote: a stored file name, and a gzipped tar.
gunzip < $f/hello.gz
gunzip < $f/gnu.tgz | lstar
# tar c then t, verbose names on stderr.
tar cvf t.tar src >[2=1]
tar tf t.tar
tar tf t.tar src/sub
# x into another directory, then archive it again: modes and mtimes came
# back, so the two archives are the same bytes.
cd out
tar xf ../t.tar
cd ..
cmp src/sub/deep/c out/src/sub/deep/c && echo trees same
cd out
tar cf ../u.tar src
cd ..
cmp t.tar u.tar && echo archives same
# z, and t recognising a gzipped archive without it.
tar czf t.tgz src
tar tf t.tgz
gunzip < t.tgz > t2.tar
cmp t.tar t2.tar && echo tgz is gzipped tar
# File mode gzip and gunzip replace the file.
cp t.tar g.tar
gzip g.tar
ls g.tar.gz
ls g.tar >[2] /dev/null || echo replaced
gunzip g.tar.gz
cmp t.tar g.tar && echo gzip file round trip
# Archives GNU tar wrote: GNU long names and pax headers.
tar tvf $f/gnu.tgz
tar tf $f/pax.tar.gz
mkdir gnu
cd gnu
tar xf $f/pax.tar.gz
cat g/hello
# Archived again, the extracted tree keeps GNU tar's modes and times.
tar cf - g | tar tv
cd ..
# k keeps existing files.
echo changed > out/src/a
cd out
tar xkf ../t.tar
cat src/a
tar xf ../t.tar src/a
cat src/a
cd ..
# puttar, gettar and lstar.
puttar src > p.tar
cd get
gettar -v < ../p.tar >[2=1]
cmp ../src/a src/a && echo gettar same
cd ..
echo src/a | puttar | tar t
# Errors.
echo junk | gunzip >[2=1]
echo $status
gunzip src/a >[2=1]
tar xf missing.tar >[2=1]
echo $status
tar tf src/a >[2=1]
tar >[2=1]
tar q >[2=1]
tar cf x.tar missing >[2=1]
echo $status
cd /
rm -r $d
