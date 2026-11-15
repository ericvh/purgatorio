# mcpfs: another MCP server's tools as files, with mcpd as the far end.
# Each tool is a directory with its schema and a call file: write the
# arguments, then read the result on the same open file.
load std
d=/tmp/mcpfs.${pid}
mkdir $d
mcpfs -m $d mcpd
cd $d
ls
ls tools | grep -c .
ls tools/run
grep -s '"serverInfo":{"name":"mcpd"' server && echo server info
grep -s '"name":"run"' tools/run/schema && echo run schema
grep -s '"inputSchema"' tools/read_file/schema && echo read_file schema
{echo '{"command":"echo hello from the far end; ns | grep -c tmp"}' >[1=0]; cat} <> tools/run/call
{echo '{"path":"/lib/probe/hello.txt"}' >[1=0]; cat} <> tools/read_file/call | grep -s 'hello from the probe root' && echo read_file result
# No arguments are {}; a call that fails says so in isError.
cat tools/job_list/call
{echo '{"path":"/nonexistent"}' >[1=0]; cat} <> tools/stat/call
# What is not a JSON object fails the read.
{echo 'not json' >[1=0]; cat} <> tools/run/call
{echo '[1]' >[1=0]; cat} <> tools/run/call
# The far end checks the arguments.
{echo '{"nonsense":1}' >[1=0]; cat} <> tools/run/call
cat tools/nosuch/call
echo x > tools/run/schema
# A call does not hold up the mount: while one sleeps, the tree is
# listed and read (mcpd itself runs its run calls one at a time).
{echo '{"command":"sleep 2; echo slow"}' >[1=0]; cat} <> tools/run/call > $d.slow &
slow=$apid
sleep 0
ls tools | grep -c .
grep -s '"name":"stat"' tools/stat/schema && echo stat schema meanwhile
grep -s '"stdout":"slow' $d.slow || echo slow call not yet answered
wait $slow
grep -s '"stdout":"slow' $d.slow && echo slow call answered
cd /
unmount $d
rm -r $d $d.slow
# With -t, a call not answered in time fails, and the mount goes on.
mkdir $d
mcpfs -t 300 -m $d mcpd
{echo '{"command":"sleep 5"}' >[1=0]; cat} <> $d/tools/run/call
ls $d/tools | grep -c .
unmount $d
rm -r $d
mcpfs -m /n nosuchcommand
echo status $status
