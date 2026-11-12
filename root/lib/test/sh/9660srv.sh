# 9660srv: ISO 9660 images built by /bin/test/mkimg, plain, with Rock
# Ridge and with Joliet, served read-only.
d=/tmp/sh-9660srv
rm -rf $d
mkdir -p $d/src/docs $d/src/deep/er $d/m
cd $d
echo 'read me' > src/docs/README.txt
echo hello > 'src/A Long Mixed-Case Name'
echo inner > src/deep/er/x
chmod 640 src/docs/README.txt
chmod 750 src/docs
chmod 644 'src/A Long Mixed-Case Name' src/deep/er/x
chmod 755 src/deep src/deep/er
/bin/test/mkimg iso plain.iso src
/bin/test/mkimg iso -r rock.iso src
/bin/test/mkimg iso -j joliet.iso src
echo plain
9660srv plain.iso m
fs ls m
ls m/docs m/deep/er
cat m/docs/readme.txt m/deep/er/x
unmount m
echo rock ridge
9660srv rock.iso m
fs ls m
cat 'm/A Long Mixed-Case Name' m/docs/README.txt
cmp src/deep/er/x m/deep/er/x && echo same
unmount m
9660srv -R rock.iso m
ls m
unmount m
echo joliet
9660srv joliet.iso m
ls m m/docs
fs ls m/docs
unmount m
9660srv -J joliet.iso m
ls m
echo union
mkdir mine
echo own > mine/own
9660srv -a rock.iso mine
ls mine
unmount mine
echo errors
echo x > m/new
rm m/docs/readme.txt
echo x >> m/deep/er/x
chmod 777 m/deep/er/x
unmount m
echo junk > junk
9660srv junk m
9660srv nosuch m
9660srv
cd /
rm -rf $d
