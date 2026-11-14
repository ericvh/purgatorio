# crypt -d refuses a password file whose Argon2id cost is above its limit
# (65536 KiB or 8 passes) before it derives anything; -C m,t with -d sets
# the limit (docs/auth.md, "crypt's password mode").
d=/tmp/sh-cryptcost
rm -rf $d
mkdir -p $d
echo cheap | crypt -C 8,1 -k pw > $d/a
crypt -d -k pw < $d/a
crypt -d -C 8,1 -k pw < $d/a
crypt -d -C 8,9 -k pw < $d/a
echo slow | crypt -C 8,9 -k pw > $d/b
crypt -d -k pw < $d/b >[2=1]
echo status $status
crypt -d -C 8,9 -k pw < $d/b
echo more memory | crypt -C 16,1 -k pw > $d/c
crypt -d -C 8,1 -k pw < $d/c >[2=1]
echo status $status
crypt -d -C 16,1 -k pw < $d/c
rm -rf $d
