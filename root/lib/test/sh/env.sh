# Variables: /env export, local variables, and the environment of
# children.
d=/tmp/sh-env
rm -rf $d
mkdir $d
shvar=exported
cat /env/shvar
echo
loc:=local
cat /env/loc >[2] /dev/null || echo loc is not exported
echo $loc
list=(a 'b c')
cat /env/list
echo
sh -c 'echo child sees $shvar'
echo -n hello > /env/fromfile
sh -c 'echo $fromfile'
shvar=()
ls /env/shvar >[2] /dev/null || echo shvar unset
rm -r $d
