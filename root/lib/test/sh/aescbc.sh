# auth/aescbc (secstore's file format) and auth/ai2key (an authinfo as a
# factotum key line).
d=/tmp/sh-aescbc
rm -rf $d
mkdir $d
cd $d
echo 'key proto=pass server=s user=glenda !password=secret' > clear
auth/aescbc -e -k 'a password' < clear > clear.aes
wc -c clear clear.aes
dd -if clear.aes -bs 16 -count 1 >[2] /dev/null
auth/aescbc -d -k 'a password' < clear.aes
echo 'a password' > keyfile
auth/aescbc -d -f keyfile < clear.aes | cmp - clear && echo same
auth/aescbc -e -f keyfile < clear.aes | auth/aescbc -d -f keyfile | cmp - clear.aes && echo twice
auth/aescbc -d -k wrong < clear.aes
dd -if clear.aes -bs 40 -count 1 >[2] /dev/null > short
auth/aescbc -d -k 'a password' < short
cat /dev/null | auth/aescbc -e -k k | auth/aescbc -d -k k | wc -c
auth/aescbc -d -e -k x
auth/aescbc -k x
auth/aescbc -d -f nosuch
echo ai2key
auth/createsignerkey -f sk signer > /dev/null
auth/mkauthinfo -f sk -e 0 glenda $d/glenda.ai
auth/ai2key -t 'note=test' $d/glenda.ai | sed 's/!seed=[0-9a-f]+/!seed=HEX/; s/cert=.*/cert=CERT/'
auth/ai2key $d/glenda.ai | grep -c 'cert=''pgcert1 glenda [0-9a-f]+ 0 [0-9a-f:]+'''
auth/ai2key $d/nosuch
auth/ai2key
cd /
rm -rf $d
