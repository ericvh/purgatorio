# #^ is the snarf buffer, as in Inferno's emu: one file, snarf, bound on
# /dev. What is written replaces the buffer when the file is closed, all
# the writes of one open together, and a read returns the buffer.
ls '#^'
echo hello > /dev/snarf
cat /dev/snarf
{echo one; echo two} > /dev/snarf
cat '#^/snarf'
echo -n > /dev/snarf
cat /dev/snarf
echo empty
echo -n last > /dev/snarf
cat /dev/snarf
echo
echo -n > /dev/snarf
