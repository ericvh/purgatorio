# bit2gif and gif2bit: a 4x2 two-bit grey image to a GIF (four greys in
# its table) and back, as an m8 image with the nearest rgbv colours; the
# names they make, and their messages (on standard output, as Inferno's).
d=/tmp/sh-gif
rm -rf $d
mkdir -p $d
cp /lib/test/gif/tiny.bit $d/tiny.bit
bit2gif $d/tiny.bit
xd $d/tiny.gif
gif2bit $d/tiny.gif
xd $d/tiny.bit
cp $d/tiny.bit $d/again.bit
bit2gif $d/again.bit
wc -c < $d/again.gif
gif2bit $d/again.gif
cmp $d/again.bit $d/tiny.bit && echo m8 round trip
cp /lib/test/gif/tiny.bit $d/plain
bit2gif $d/plain
ls $d/plain.gif
bit2gif
gif2bit -x
bit2gif $d/missing.bit
gif2bit $d/missing.gif
echo junk > $d/junk.gif
gif2bit $d/junk.gif
rm -rf $d
