// gitalybench drives Gitaly the way starhub-server does: through starhub's own Gitaly client
// (builder/git/gitserver/gitaly), plus raw gitalypb calls for what that client does not wrap.
//
//	gitalybench serve            git smart HTTP proxy on :18081 (starhub's InfoRefs/UploadPack/ReceivePack)
//	                             and the internal hook API stub on :18080 (allowed/pre_receive/post_receive)
//	gitalybench import BUNDLE    CreateRepositoryFromBundle
//	gitalybench run PLAN OUT     run the read and write workload, write results as JSON
package main

import (
	"bytes"
	"compress/gzip"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
	"sort"
	"strings"
	"sync"
	"time"

	gitalyauth "gitlab.com/gitlab-org/gitaly/v16/auth"
	gitalypb "gitlab.com/gitlab-org/gitaly/v16/proto/go/gitalypb"
	"gitlab.com/gitlab-org/gitaly/v16/streamio"
	"google.golang.org/grpc"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"opencsg.com/csghub-server/builder/git/gitserver"
	"opencsg.com/csghub-server/builder/git/gitserver/gitaly"
	"opencsg.com/csghub-server/common/config"
	"opencsg.com/csghub-server/common/types"
)

const (
	addr      = "tcp://127.0.0.1:18075"
	token     = "abc123secret"
	storage   = "default"
	repoType  = types.RepositoryType("model") // EmptyRepoType: no starhub database lookup
	namespace = "opencsg"
	name      = "starhub"
	relPath   = "models_opencsg/starhub.git" // starhub's BuildRelativePath: lower("<type>s_<namespace>/<name>") + ".git"
)

func cfg() *config.Config {
	c := &config.Config{}
	c.GitalyServer.Address = addr
	c.GitalyServer.Token = token
	c.GitalyServer.Storage = storage
	c.RepoTemplate.EmptyRepoType = string(repoType)
	c.Git.OperationTimeout = 600
	c.Git.TreeOperationTimeout = 600
	c.Git.FetchTimeout = 600
	c.Git.MaxArchiveSizeMB = 50 // starhub's default (STARHUB_SERVER_GIT_MAX_ARCHIVE_SIZE_MB)
	return c
}

func repo() *gitalypb.Repository {
	return &gitalypb.Repository{StorageName: storage, RelativePath: relPath, GlRepository: "models/opencsg/starhub"}
}

// raw gitalypb connection, with the same auth and routing metadata as starhub's client.
func rawConn() *grpc.ClientConn {
	info, _ := json.Marshal(map[string]any{storage: map[string]string{"address": addr, "token": token}})
	md := metadata.New(map[string]string{"gitaly-servers": base64.StdEncoding.EncodeToString(info)})
	conn, err := grpc.NewClient(strings.TrimPrefix(addr, "tcp://"),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithPerRPCCredentials(gitalyauth.RPCCredentialsV2(token)),
		grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(256<<20)),
		grpc.WithUnaryInterceptor(func(ctx context.Context, m string, req, reply any, cc *grpc.ClientConn, inv grpc.UnaryInvoker, o ...grpc.CallOption) error {
			return inv(metadata.NewOutgoingContext(ctx, md), m, req, reply, cc, o...)
		}),
		grpc.WithStreamInterceptor(func(ctx context.Context, d *grpc.StreamDesc, cc *grpc.ClientConn, m string, s grpc.Streamer, o ...grpc.CallOption) (grpc.ClientStream, error) {
			return s(metadata.NewOutgoingContext(ctx, md), d, cc, m, o...)
		}))
	if err != nil {
		log.Fatal(err)
	}
	return conn
}

// GetTreeEntries names the root "."
func pathOrDot(p string) string {
	if p == "" {
		return "."
	}
	return p
}

func must(err error) {
	if err != nil {
		log.Fatal(err)
	}
}

// ------------------------------------------------------------------------------ serve
func serve() {
	c, err := gitaly.NewClient(cfg())
	must(err)
	hook := http.NewServeMux()
	ok := func(body string) http.HandlerFunc {
		return func(w http.ResponseWriter, r *http.Request) {
			b, _ := io.ReadAll(r.Body)
			log.Printf("hook %s %s %.300s", r.Method, r.URL.Path, b)
			w.Header().Set("Content-Type", "application/json")
			_, _ = w.Write([]byte(body))
		}
	}
	// Same responses as starhub's InternalHandler
	hook.HandleFunc("/api/v4/internal/allowed", ok(`{"status":true,"message":"allowed"}`))
	hook.HandleFunc("/api/v4/internal/pre_receive", ok(`{"reference_counter_increased":true}`))
	hook.HandleFunc("/api/v4/internal/post_receive", ok(`{"reference_counter_decreased":true,"messages":[]}`))
	hook.HandleFunc("/api/v4/internal/check", ok(`{"api_version":"v4","redis":true}`))
	hook.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		log.Printf("hook unhandled %s %s", r.Method, r.URL.Path)
		http.NotFound(w, r)
	})
	go func() { must(http.ListenAndServe(":28090", hook)) }()

	git := http.NewServeMux()
	git.HandleFunc("/starhub.git/info/refs", func(w http.ResponseWriter, r *http.Request) {
		rpc := r.URL.Query().Get("service")
		w.Header().Set("Content-Type", fmt.Sprintf("application/x-%s-advertisement", rpc))
		w.Header().Set("Cache-Control", "no-cache")
		rd, err := c.InfoRefsResponse(r.Context(), gitserver.InfoRefsReq{
			Namespace: namespace, Name: name, RepoType: repoType, Rpc: rpc, GitProtocol: r.Header.Get("Git-Protocol"),
		})
		if err != nil {
			http.Error(w, err.Error(), 500)
			return
		}
		_, _ = io.Copy(w, rd)
	})
	body := func(r *http.Request) {
		if r.Header.Get("Content-Encoding") == "gzip" {
			if z, err := gzip.NewReader(r.Body); err == nil {
				r.Body = z
			}
		}
	}
	git.HandleFunc("/starhub.git/git-upload-pack", func(w http.ResponseWriter, r *http.Request) {
		body(r)
		w.Header().Set("Content-Type", "application/x-git-upload-pack-result")
		if err := c.UploadPack(r.Context(), gitserver.UploadPackReq{
			Namespace: namespace, Name: name, RepoType: repoType, GitProtocol: r.Header.Get("Git-Protocol"), Request: r, Writer: w,
		}); err != nil {
			log.Printf("upload-pack: %v", err)
		}
	})
	git.HandleFunc("/starhub.git/git-receive-pack", func(w http.ResponseWriter, r *http.Request) {
		body(r)
		w.Header().Set("Content-Type", "application/x-git-receive-pack-result")
		if err := c.ReceivePack(r.Context(), gitserver.ReceivePackReq{
			Namespace: namespace, Name: name, RepoType: repoType, GitProtocol: r.Header.Get("Git-Protocol"), Request: r, Writer: w,
			UserId: 1, Username: "tester",
		}); err != nil {
			log.Printf("receive-pack: %v", err)
		}
	})
	log.Println("serving git on :18081, hooks on :28090")
	must(http.ListenAndServe("0.0.0.0:18081", git))
}

// ------------------------------------------------------------------------------ import
func importBundle(path string) {
	conn := rawConn()
	rc := gitalypb.NewRepositoryServiceClient(conn)
	ctx := context.Background()
	_, _ = rc.RemoveRepository(ctx, &gitalypb.RemoveRepositoryRequest{Repository: repo()})
	f, err := os.Open(path)
	must(err)
	defer f.Close()
	t := time.Now()
	stream, err := rc.CreateRepositoryFromBundle(ctx)
	must(err)
	must(stream.Send(&gitalypb.CreateRepositoryFromBundleRequest{Repository: repo()}))
	w := streamio.NewWriter(func(p []byte) error {
		return stream.Send(&gitalypb.CreateRepositoryFromBundleRequest{Data: p})
	})
	_, err = io.Copy(w, f)
	must(err)
	_, err = stream.CloseAndRecv()
	must(err)
	// starhub sets HEAD to the default branch (SetDefaultBranch)
	c, err := gitaly.NewClient(cfg())
	must(err)
	must(c.SetDefaultBranch(ctx, gitserver.SetDefaultBranchReq{Namespace: namespace, Name: name, RepoType: repoType, BranchName: "main"}))
	fmt.Printf("{\"import_ms\": %d}\n", time.Since(t).Milliseconds())
}

// ------------------------------------------------------------------------------ run
type RevPath struct {
	Rev  string `json:"rev"`
	Path string `json:"path"`
}
type Pair struct {
	A string `json:"a"`
	B string `json:"b"`
}
type Plan struct {
	Files      []RevPath `json:"files"`
	Dirs       []RevPath `json:"dirs"`
	Logs       []string  `json:"logs"`
	PathLogs   []RevPath `json:"path_logs"`
	Commits    []string  `json:"commits"`
	Diffs      []Pair    `json:"diffs"`
	Blames     []RevPath `json:"blames"`
	Archives   []string  `json:"archives"`
	WriteFiles []string  `json:"write_files"` // existing paths to edit
	Concurrent int       `json:"concurrent"`
	PerWorker  int       `json:"per_worker"`
	BatchSize  int       `json:"batch_size"`
	Levels     []int     `json:"reader_levels"`
	LargeMB    int       `json:"large_file_mb"`
}

type Result struct {
	Timings map[string][]float64 `json:"timings"` // op -> ms per call
	Values  map[string]any       `json:"values"`  // op -> outputs for correctness checks
	Errors  map[string][]string  `json:"errors"`
	mu      sync.Mutex
}

func (r *Result) time(op string, f func() error) {
	t := time.Now()
	err := f()
	ms := float64(time.Since(t).Microseconds()) / 1000
	r.mu.Lock()
	defer r.mu.Unlock()
	r.Timings[op] = append(r.Timings[op], ms)
	if err != nil {
		r.Errors[op] = append(r.Errors[op], err.Error())
	}
}

func (r *Result) put(op, key string, v any) {
	r.mu.Lock()
	defer r.mu.Unlock()
	m, _ := r.Values[op].(map[string]any)
	if m == nil {
		m = map[string]any{}
		r.Values[op] = m
	}
	m[key] = v
}

func digest(b []byte) string { h := sha256.Sum256(b); return hex.EncodeToString(h[:]) }

func run(planPath, outPath string) {
	var plan Plan
	b, err := os.ReadFile(planPath)
	must(err)
	must(json.Unmarshal(b, &plan))
	c, err := gitaly.NewClient(cfg())
	must(err)
	conn := rawConn()
	commitC := gitalypb.NewCommitServiceClient(conn)
	diffC := gitalypb.NewDiffServiceClient(conn)
	ctx := context.Background()
	res := &Result{Timings: map[string][]float64{}, Values: map[string]any{}, Errors: map[string][]string{}}
	base := func(rev, path string) gitserver.GetRepoInfoByPathReq {
		return gitserver.GetRepoInfoByPathReq{Namespace: namespace, Name: name, RepoType: repoType, Ref: rev, Path: path}
	}

	for _, f := range plan.Files {
		res.time("read_file", func() error {
			s, err := c.GetRepoFileRaw(ctx, base(f.Rev, f.Path))
			res.put("read_file", f.Rev+":"+f.Path, digest([]byte(s)))
			return err
		})
	}
	for _, d := range plan.Dirs {
		res.time("list_dir", func() error {
			files, err := c.GetRepoFileTree(ctx, base(d.Rev, d.Path))
			names := []string{}
			for _, x := range files {
				names = append(names, x.Name)
			}
			sort.Strings(names)
			res.put("list_dir", d.Rev+":"+d.Path, names)
			return err
		})
		res.time("last_commits", func() error {
			lt, err := c.GetLogsTree(ctx, types.GetLogsTreeRequest{Namespace: namespace, Name: name, RepoType: repoType, Ref: d.Rev, Path: d.Path, Limit: 1000})
			m := map[string]string{}
			if lt != nil {
				for _, x := range lt.Commits {
					m[x.Name] = x.ID
				}
			}
			res.put("last_commits", d.Rev+":"+d.Path, m)
			return err
		})
	}
	// The same reads through raw Gitaly RPCs, without starhub's extras (last commits and blob
	// contents for a listing, the patch for a commit), to match what the other systems do
	for _, d := range plan.Dirs {
		res.time("list_dir_raw", func() error {
			st, err := commitC.GetTreeEntries(ctx, &gitalypb.GetTreeEntriesRequest{Repository: repo(), Revision: []byte(d.Rev), Path: []byte(pathOrDot(d.Path))})
			if err != nil {
				return err
			}
			names := []string{}
			for {
				r, err := st.Recv()
				if err == io.EOF {
					break
				}
				if err != nil {
					return err
				}
				for _, x := range r.Entries {
					p := string(x.Path)
					names = append(names, p[strings.LastIndex(p, "/")+1:])
				}
			}
			sort.Strings(names)
			res.put("list_dir_raw", d.Rev+":"+d.Path, names)
			return nil
		})
	}
	for _, rev := range plan.Commits {
		res.time("show_commit_raw", func() error {
			cr, err := commitC.FindCommit(ctx, &gitalypb.FindCommitRequest{Repository: repo(), Revision: []byte(rev)})
			if err != nil {
				return err
			}
			left := "4b825dc642cb6eb9a060e54bf8d69288fbee4904" // empty tree
			if len(cr.Commit.ParentIds) > 0 {
				left = cr.Commit.ParentIds[0]
			}
			st, err := diffC.FindChangedPaths(ctx, &gitalypb.FindChangedPathsRequest{Repository: repo(), Requests: []*gitalypb.FindChangedPathsRequest_Request{{Type: &gitalypb.FindChangedPathsRequest_Request_TreeRequest_{TreeRequest: &gitalypb.FindChangedPathsRequest_Request_TreeRequest{LeftTreeRevision: left, RightTreeRevision: rev}}}}})
			if err != nil {
				return err
			}
			for {
				if _, err := st.Recv(); err == io.EOF {
					break
				} else if err != nil {
					return err
				}
			}
			res.put("show_commit_raw", rev, cr.Commit.Id)
			return nil
		})
	}
	for _, rev := range plan.Logs {
		res.time("log_50", func() error {
			cs, _, err := c.GetRepoCommits(ctx, gitserver.GetRepoCommitsReq{Namespace: namespace, Name: name, RepoType: repoType, Ref: rev, Per: 50, Page: 1})
			ids := []string{}
			for _, x := range cs {
				ids = append(ids, x.ID)
			}
			res.put("log_50", rev, ids)
			return err
		})
	}
	for _, p := range plan.PathLogs {
		res.time("path_log_50", func() error {
			st, err := commitC.FindCommits(ctx, &gitalypb.FindCommitsRequest{Repository: repo(), Revision: []byte(p.Rev), Paths: [][]byte{[]byte(p.Path)}, Limit: 50, Order: gitalypb.FindCommitsRequest_TOPO})
			if err != nil {
				return err
			}
			ids := []string{}
			for {
				r, err := st.Recv()
				if err == io.EOF {
					break
				}
				if err != nil {
					return err
				}
				for _, x := range r.Commits {
					ids = append(ids, x.Id)
				}
			}
			res.put("path_log_50", p.Rev+":"+p.Path, ids)
			return nil
		})
	}
	for _, rev := range plan.Commits {
		res.time("show_commit", func() error {
			cr, err := c.GetSingleCommit(ctx, gitserver.GetRepoLastCommitReq{Namespace: namespace, Name: name, RepoType: repoType, Ref: rev})
			if cr != nil {
				res.put("show_commit", rev, cr.Commit.ID)
			}
			return err
		})
	}
	for _, d := range plan.Diffs {
		res.time("diff_names", func() error {
			out, err := c.GetDiffBetweenTwoCommits(ctx, gitserver.GetDiffBetweenTwoCommitsReq{Namespace: namespace, Name: name, RepoType: repoType, Ref: "main", LeftCommitId: d.A, RightCommitId: d.B})
			names := []string{}
			if out != nil {
				for _, cm := range out.Commits {
					names = append(names, cm.Added...)
					names = append(names, cm.Modified...)
					names = append(names, cm.Removed...)
				}
			}
			sort.Strings(names)
			res.put("diff_names", d.A+".."+d.B, names)
			return err
		})
	}
	for _, bl := range plan.Blames {
		res.time("blame", func() error {
			st, err := commitC.RawBlame(ctx, &gitalypb.RawBlameRequest{Repository: repo(), Revision: []byte(bl.Rev), Path: []byte(bl.Path)})
			if err != nil {
				return err
			}
			var buf bytes.Buffer
			for {
				r, err := st.Recv()
				if err == io.EOF {
					break
				}
				if err != nil {
					return err
				}
				buf.Write(r.Data)
			}
			// porcelain: one "<sha> <orig> <final> [n]" header per line group
			per := []string{}
			for _, l := range strings.Split(buf.String(), "\n") {
				f := strings.Fields(l)
				if len(f) >= 3 && len(f[0]) == 40 {
					per = append(per, f[0])
				}
			}
			res.put("blame", bl.Rev+":"+bl.Path, per)
			return nil
		})
	}
	for _, rev := range plan.Archives {
		res.time("archive_zip", func() error {
			data, err := c.GetArchive(ctx, gitserver.GetArchiveReq{Namespace: namespace, Name: name, RepoType: repoType, Revision: rev})
			res.put("archive_zip", rev, len(data))
			_ = os.WriteFile(outPath+".gitaly-"+rev+".zip", data, 0o644)
			return err
		})
	}
	res.time("repo_size", func() error {
		n, err := c.GetRepoSize(ctx, base("main", ""))
		res.put("repo_size", "main", n)
		return err
	})
	res.time("branches", func() error {
		bs, err := c.GetRepoBranches(ctx, gitserver.GetBranchesReq{Namespace: namespace, Name: name, RepoType: repoType, Per: 1000, Page: 1})
		res.put("branches", "count", len(bs))
		return err
	})

	// Read throughput at increasing concurrency
	for _, level := range plan.Levels {
		op := fmt.Sprintf("read_concurrency_%d", level)
		var wg sync.WaitGroup
		t := time.Now()
		for w := 0; w < level; w++ {
			wg.Add(1)
			go func(w int) {
				defer wg.Done()
				for i := w; i < len(plan.Files); i += level {
					f := plan.Files[i]
					res.time(op, func() error {
						_, err := c.GetRepoFileRaw(ctx, base(f.Rev, f.Path))
						return err
					})
				}
			}(w)
		}
		wg.Wait()
		res.put(op, "wall_ms", time.Since(t).Milliseconds())
	}

	// ---- writes (starhub's CommitFiles = UserCommitFiles, the web editor path)
	commit := func(op string, files []gitserver.CommitFile, msg string) error {
		for i := range files { // starhub's CommitFiles sends Base64Content: true
			files[i].Content = base64.StdEncoding.EncodeToString([]byte(files[i].Content))
		}
		return c.CommitFiles(ctx, gitserver.CommitFilesReq{Namespace: namespace, Name: name, RepoType: repoType, Revision: "main", Username: "tester", Email: "tester@example.com", Message: msg, Files: files})
	}
	for i, p := range plan.WriteFiles {
		res.time("commit_1_file", func() error {
			return commit("commit_1_file", []gitserver.CommitFile{{Path: p, Content: fmt.Sprintf("edited %d\n", i), Action: gitserver.CommitActionUpdate}}, fmt.Sprintf("edit %d", i))
		})
	}
	for round := 0; round < 3; round++ {
		files := []gitserver.CommitFile{}
		for i := 0; i < plan.BatchSize; i++ {
			files = append(files, gitserver.CommitFile{Path: fmt.Sprintf("bench/batch%d/f%d.txt", round, i), Content: fmt.Sprintf("batch %d file %d\n", round, i), Action: gitserver.CommitActionCreate})
		}
		res.time("commit_batch", func() error { return commit("commit_batch", files, fmt.Sprintf("batch %d", round)) })
	}
	big := make([]byte, plan.LargeMB<<20)
	for i := range big {
		big[i] = byte((i * 2654435761) >> 13)
	}
	res.time("commit_large_file", func() error {
		return commit("commit_large_file", []gitserver.CommitFile{{Path: "bench/large.bin", Content: string(big), Action: gitserver.CommitActionCreate}}, "large file")
	})
	// Concurrent writers to one branch: each creates its own files
	var wg sync.WaitGroup
	t := time.Now()
	for w := 0; w < plan.Concurrent; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			for i := 0; i < plan.PerWorker; i++ {
				res.time("concurrent_commit", func() error {
					return commit("concurrent_commit", []gitserver.CommitFile{{Path: fmt.Sprintf("bench/w%d/c%d.txt", w, i), Content: "x\n", Action: gitserver.CommitActionCreate}}, fmt.Sprintf("w%d c%d", w, i))
				})
			}
		}(w)
	}
	wg.Wait()
	res.put("concurrent_commit", "wall_ms", time.Since(t).Milliseconds())
	// How many of the concurrent commits' files survived on main
	files, err := c.GetRepoFileTree(ctx, base("main", "bench"))
	survived := 0
	if err == nil {
		for _, d := range files {
			if strings.HasPrefix(d.Name, "w") {
				sub, _ := c.GetRepoFileTree(ctx, base("main", "bench/"+d.Name))
				survived += len(sub)
			}
		}
	}
	res.put("concurrent_commit", "files_on_main", survived)
	tip, err := commitC.FindCommit(ctx, &gitalypb.FindCommitRequest{Repository: repo(), Revision: []byte("main")})
	must(err)
	for i := 0; i < 20; i++ {
		res.time("branch_create", func() error {
			return c.CreateBranch(ctx, gitserver.CreateBranchReq{Namespace: namespace, Name: name, RepoType: repoType, BranchName: fmt.Sprintf("bench-%d", i), CommitID: tip.Commit.Id})
		})
	}
	for i := 0; i < 20; i++ {
		res.time("branch_delete", func() error {
			return c.DeleteRepoBranch(ctx, gitserver.DeleteBranchReq{Namespace: namespace, Name: name, RepoType: repoType, Ref: fmt.Sprintf("bench-%d", i), Username: "tester", Email: "tester@example.com"})
		})
	}
	out, _ := json.MarshalIndent(res, "", " ")
	must(os.WriteFile(outPath, out, 0o644))
	fmt.Println("done")
}

func main() {
	log.SetFlags(0)
	switch os.Args[1] {
	case "serve":
		serve()
	case "import":
		importBundle(os.Args[2])
	case "run":
		run(os.Args[2], os.Args[3])
	}
}
