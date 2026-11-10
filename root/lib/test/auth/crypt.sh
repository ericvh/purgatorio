# crypt, auth/secstore and netkey (docs/auth.md).
d=/tmp/auth-crypt
rm -rf $d
mkdir -p $d
echo attack at dawn | crypt -C 8,1 -k key > $d/c
crypt -d -k key < $d/c
crypt -d -k nope < $d/c
echo key > $d/keyfile
crypt -d -f $d/keyfile < $d/c
echo not encrypted | crypt -d -k key
echo topsecret > $d/plain
echo pw | auth/secstore -s $d/store -C 8,1 -p $d/plain
auth/secstore -s $d/store
echo pw | auth/secstore -s $d/store -G plain
echo bad | auth/secstore -s $d/store -G plain
cd $d
echo pw | auth/secstore -s $d/store -g plain
cat $d/plain
auth/secstore -s $d/store -r plain
auth/secstore -s $d/store | wc -l
{echo password; echo 12345; echo 54321} | netkey
cd /
rm -rf $d
