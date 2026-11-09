// Captures real 9P2000 traffic produced by an independent implementation,
// 9fans.net/go/plan9: T-messages come from its client package, R-messages
// are marshalled by its Fcall encoder in a small file server. Each message
// is written to <out>/NN-<type>.bin in wire order.
package main

import (
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"

	"9fans.net/go/plan9"
	"9fans.net/go/plan9/client"
)

var names = map[byte]string{
	100: "Tversion", 101: "Rversion", 102: "Tauth", 103: "Rauth", 104: "Tattach", 105: "Rattach",
	107: "Rerror", 108: "Tflush", 109: "Rflush", 110: "Twalk", 111: "Rwalk", 112: "Topen", 113: "Ropen",
	114: "Tcreate", 115: "Rcreate", 116: "Tread", 117: "Rread", 118: "Twrite", 119: "Rwrite",
	120: "Tclunk", 121: "Rclunk", 122: "Tremove", 123: "Rremove", 124: "Tstat", 125: "Rstat",
	126: "Twstat", 127: "Rwstat",
}

type recorder struct {
	out string
	n   *int
	buf []byte
}

func (r *recorder) feed(p []byte) {
	r.buf = append(r.buf, p...)
	for len(r.buf) >= 4 {
		size := int(binary.LittleEndian.Uint32(r.buf))
		if len(r.buf) < size {
			return
		}
		msg := r.buf[:size]
		*r.n++
		name := fmt.Sprintf("%02d-%s.bin", *r.n, names[msg[4]])
		must(os.WriteFile(filepath.Join(r.out, name), msg, 0o644))
		r.buf = r.buf[size:]
	}
}

type tap struct {
	net.Conn
	rec *recorder
}

func (t *tap) Read(p []byte) (int, error) {
	n, err := t.Conn.Read(p)
	t.rec.feed(p[:n])
	return n, err
}

func (t *tap) Write(p []byte) (int, error) {
	t.rec.feed(p)
	return t.Conn.Write(p)
}

type node struct {
	dir  plan9.Dir
	data []byte
	kids []string
}

func mkdir(name string, path uint64) *node {
	return &node{dir: plan9.Dir{Type: 'M', Dev: 1, Qid: plan9.Qid{Path: path, Type: plan9.QTDIR},
		Mode: plan9.DMDIR | 0o755, Atime: 1700000000, Mtime: 1700000000, Name: name,
		Uid: "glenda", Gid: "glenda", Muid: "glenda"}}
}

func serve(c net.Conn) {
	files := map[string]*node{"/": mkdir("/", 0), "/sub": mkdir("sub", 2)}
	hello := []byte("hello, world\n")
	files["/hello"] = &node{data: hello, dir: plan9.Dir{Type: 'M', Dev: 1,
		Qid: plan9.Qid{Path: 1, Vers: 3}, Mode: 0o644, Atime: 1700000000, Mtime: 1700000001,
		Length: uint64(len(hello)), Name: "hello", Uid: "glenda", Gid: "glenda", Muid: "glenda"}}
	files["/"].kids = []string{"/hello", "/sub"}
	fids := map[uint32]string{}
	for {
		t, err := plan9.ReadFcall(c)
		if err != nil {
			return
		}
		r := &plan9.Fcall{Tag: t.Tag, Type: t.Type + 1}
		fail := func(e string) { r.Type = plan9.Rerror; r.Ename = e }
		switch t.Type {
		case plan9.Tversion:
			r.Msize, r.Version = t.Msize, "9P2000"
		case plan9.Tattach:
			fids[t.Fid] = "/"
			r.Qid = files["/"].dir.Qid
		case plan9.Twalk:
			p := fids[t.Fid]
			for _, w := range t.Wname {
				if w == ".." {
					p = "/"
				} else if p == "/" {
					p = "/" + w
				} else {
					p = p + "/" + w
				}
				n, ok := files[p]
				if !ok {
					break
				}
				r.Wqid = append(r.Wqid, n.dir.Qid)
			}
			if len(r.Wqid) < len(t.Wname) {
				if len(r.Wqid) == 0 {
					fail("file does not exist")
				}
			} else {
				fids[t.Newfid] = p
			}
		case plan9.Topen:
			r.Qid, r.Iounit = files[fids[t.Fid]].dir.Qid, 8192
		case plan9.Tcreate:
			fail("permission denied")
		case plan9.Tread:
			n := files[fids[t.Fid]]
			var all []byte
			if n.dir.Qid.Type&plan9.QTDIR != 0 {
				for _, k := range n.kids {
					b, _ := files[k].dir.Bytes()
					all = append(all, b...)
				}
			} else {
				all = n.data
			}
			if t.Offset < uint64(len(all)) {
				all = all[t.Offset:]
				if uint32(len(all)) > t.Count {
					all = all[:t.Count]
				}
				r.Data = all
			}
		case plan9.Tstat:
			r.Stat, _ = files[fids[t.Fid]].dir.Bytes()
		case plan9.Tclunk:
			delete(fids, t.Fid)
		default:
			fail("not implemented")
		}
		must(plan9.WriteFcall(c, r))
	}
}

func main() {
	out := os.Args[1]
	srv, cli := net.Pipe()
	go serve(srv)

	n := 0
	conn, err := client.NewConn(&tap{Conn: cli, rec: &recorder{out: out, n: &n}})
	must(err)
	root, err := conn.Attach(nil, "glenda", "")
	must(err)

	f, err := root.Open("hello", plan9.OREAD)
	must(err)
	data, err := io.ReadAll(f)
	must(err)
	if string(data) != "hello, world\n" {
		panic(string(data))
	}
	_, err = f.Stat()
	must(err)
	f.Close()

	if _, err = root.Stat("nope"); err == nil {
		panic("stat nope succeeded")
	}
	if _, err = root.Create("sub/new", plan9.OWRITE, 0o644); err == nil {
		panic("create succeeded")
	}

	d, err := root.Open(".", plan9.OREAD)
	must(err)
	dirs, err := d.Dirreadall()
	must(err)
	if len(dirs) != 2 {
		panic(len(dirs))
	}
	d.Close()
	conn.Close()
}

func must(err error) {
	if err != nil {
		panic(err)
	}
}
