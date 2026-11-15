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
# Resources: the list as JSON, and a resource's contents by its URI.
grep -s '"resources"' resources/list && echo resource list
{echo purg:///lib/probe/hello.txt >[1=0]; cat} <> resources/read
{echo purg:///nonexistent >[1=0]; cat} <> resources/read
cat resources/read
echo x > resources/list
# Prompts: the list as JSON, and one prompt by its parameters.
grep -s '"name":"inferno-sh"' prompts/list && echo prompt list
{echo '{"name":"inferno-sh"}' >[1=0]; cat} <> prompts/get | grep -s '"role":"user"' && echo prompt
{echo '{"name":"nosuch"}' >[1=0]; cat} <> prompts/get
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
# A server that says its tools changed is asked for them again: tool a
# goes and b comes (/lib/test/mcp/changing).
mkdir $d
mcpfs -m $d sh /lib/test/mcp/changing
for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 {test -d $d/tools/b || sleep 0.5}
ls $d/tools | sed 's/.*\///'
test -d $d/tools/a || echo a gone
unmount $d
rm -r $d
mcpfs -m /n nosuchcommand
echo status $status
