# crypt's public-key mode (docs/auth.md, "crypt's public-key mode"):
# crypt -g makes a hybrid X25519 + ML-KEM-768 key pair, crypt -r encrypts
# to its public key, and crypt -d decrypts with the secret key file (-s)
# or with the key loaded into factotum. The wrong key, a secret instead
# of a key, a key instead of a secret and a damaged file are refused.
d=/tmp/sh-pqcrypt
rm -rf $d
mkdir -p $d
crypt -g $d/key > $d/key.pub
sed 's/ .*//' $d/key.pub
wc -c < $d/key.pub
auth/getpk $d/key | cmp - $d/key.pub && echo getpk agrees
echo attack at dawn | crypt -r $d/key.pub > $d/c
crypt -d -s $d/key < $d/c
echo to the secret key file | crypt -r $d/key | crypt -d -s $d/key
crypt -g $d/other > $d/other.pub
crypt -d -s $d/other < $d/c
crypt -d -k key < $d/c
echo a secret | crypt -C 8,1 -k pw > $d/p
crypt -d -s $d/key < $d/p
crypt -d -k pw < $d/p
dd -if $d/c -of $d/t -bs 1170 -count 1 >[2] /dev/null
crypt -d -s $d/key < $d/t
crypt -r $d/key.pub -k pw < /dev/null
echo factotum
mkdir -p /mnt/factotum
auth/factotum $d/key
sed 's/ id=.* !/ !/' /mnt/factotum/ctl
crypt -d < $d/c
echo for another | crypt -r $d/other.pub | crypt -d
unmount /mnt/factotum
crypt -d < $d/c
rm -rf $d
