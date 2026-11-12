# dbm/store, fetch, delete, keys and list on a database made empty here.
load std
d=/tmp/sh-dbm
rm -rf $d
mkdir -p $d
cd $d
touch db.dir db.pag
dbm/store db one 1
echo 'two	2 with a tab before it
three three, after a space
four 4' | dbm/store db
dbm/fetch db one
echo
dbm/fetch db three
echo
dbm/keys db | sort
dbm/list db | sort
echo errors
dbm/store db one again
echo $status
dbm/fetch db nosuch
dbm/delete db two
dbm/delete db two
dbm/keys db | sort
echo 'five 5
noseparator
six 6' | dbm/store db
echo $status
dbm/fetch db six
echo many
ls /bin/*.wasm /bin/*/*.wasm | sed 's/$/ a value long enough to fill blocks soon/' | dbm/store db
ls /bin/*.wasm /bin/*/*.wasm | sort > want
dbm/keys db | grep wasm | sort > got
cmp want got && echo same keys
dbm/fetch db /bin/sh.wasm
echo
dbm/fetch nodb one
dbm/fetch
cd /
rm -rf $d
