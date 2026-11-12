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
cd /
unmount $d
rm -r $d
mcpfs -m /n nosuchcommand
echo status $status
