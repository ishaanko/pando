// gogit implements the Pando compat helper contract on top of go-git.
//
//	gogit clone <url> <dir>
//	gogit fetch <dir>
//	gogit push <dir> <refspec>...
//
// http(s) URLs carry credentials in the userinfo (basic auth); ssh URLs use the
// unencrypted private key at $PANDO_SSH_KEY with host key checking disabled.
package main

import (
	"errors"
	"fmt"
	"net/url"
	"os"

	git "github.com/go-git/go-git/v5"
	"github.com/go-git/go-git/v5/config"
	"github.com/go-git/go-git/v5/plumbing/transport"
	"github.com/go-git/go-git/v5/plumbing/transport/http"
	gitssh "github.com/go-git/go-git/v5/plumbing/transport/ssh"
	cryptossh "golang.org/x/crypto/ssh"
)

// authFor derives transport credentials from the remote URL.
func authFor(rawURL string) (transport.AuthMethod, error) {
	u, err := url.Parse(rawURL)
	if err != nil {
		return nil, err
	}
	switch u.Scheme {
	case "http", "https":
		if u.User == nil {
			return nil, nil
		}
		pass, _ := u.User.Password()
		return &http.BasicAuth{Username: u.User.Username(), Password: pass}, nil
	case "ssh":
		key := os.Getenv("PANDO_SSH_KEY")
		if key == "" {
			return nil, errors.New("PANDO_SSH_KEY is not set")
		}
		user := "git"
		if u.User != nil {
			user = u.User.Username()
		}
		keys, err := gitssh.NewPublicKeysFromFile(user, key, "")
		if err != nil {
			return nil, err
		}
		keys.HostKeyCallback = cryptossh.InsecureIgnoreHostKey()
		return keys, nil
	}
	return nil, fmt.Errorf("unsupported URL scheme %q", u.Scheme)
}

// originURL reads the origin URL (credentials included) from the repo config.
func originURL(repo *git.Repository) (string, error) {
	remote, err := repo.Remote("origin")
	if err != nil {
		return "", err
	}
	return remote.Config().URLs[0], nil
}

func run(args []string) error {
	if len(args) == 0 {
		return errors.New("usage: gogit clone <url> <dir> | fetch <dir> | push <dir> <refspec>...")
	}
	switch args[0] {
	case "clone":
		if len(args) != 3 {
			return errors.New("usage: gogit clone <url> <dir>")
		}
		auth, err := authFor(args[1])
		if err != nil {
			return err
		}
		_, err = git.PlainClone(args[2], false, &git.CloneOptions{URL: args[1], Auth: auth})
		return err
	case "fetch", "push":
		if len(args) < 2 {
			return errors.New("usage: gogit fetch <dir> | push <dir> <refspec>...")
		}
		repo, err := git.PlainOpen(args[1])
		if err != nil {
			return err
		}
		rawURL, err := originURL(repo)
		if err != nil {
			return err
		}
		auth, err := authFor(rawURL)
		if err != nil {
			return err
		}
		if args[0] == "fetch" {
			err = repo.Fetch(&git.FetchOptions{RemoteName: "origin", Auth: auth})
		} else {
			if len(args) < 3 {
				return errors.New("push needs at least one refspec")
			}
			specs := make([]config.RefSpec, 0, len(args)-2)
			for _, s := range args[2:] {
				specs = append(specs, config.RefSpec(s))
			}
			err = repo.Push(&git.PushOptions{RemoteName: "origin", RefSpecs: specs, Auth: auth})
		}
		if errors.Is(err, git.NoErrAlreadyUpToDate) {
			return nil
		}
		return err
	}
	return fmt.Errorf("unknown subcommand %q", args[0])
}

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, "gogit:", err)
		os.Exit(1)
	}
}
