// Adapted from anomalyco/opencode's permission/arity.ts.
// Copyright 2025 opencode. Licensed under MIT.
// Modified for WeCode: represented as a Rust match table.

pub(crate) fn prefix(tokens: &[String]) -> Vec<String> {
    for len in (1..=tokens.len()).rev() {
        let candidate = tokens[..len]
            .iter()
            .map(|token| token.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(arity) = arity(&candidate) {
            return tokens[..tokens.len().min(arity)].to_vec();
        }
    }
    tokens.first().cloned().into_iter().collect()
}

fn arity(command: &str) -> Option<usize> {
    Some(match command {
        "cat" | "cd" | "chmod" | "chown" | "cp" | "echo" | "env" | "export" | "grep" | "kill"
        | "killall" | "ln" | "ls" | "mkdir" | "mv" | "ps" | "pwd" | "rm" | "rmdir" | "sleep"
        | "source" | "tail" | "touch" | "unset" | "which" => 1,
        "aws" | "az" | "doctl" | "gcloud" | "gh" | "sfdx" => 3,
        "bazel" | "brew" | "bun" | "cargo" | "cdk" | "cf" | "cmake" | "composer" | "consul"
        | "crictl" | "deno" | "docker" | "eksctl" | "firebase" | "flyctl" | "git" | "go"
        | "gradle" | "helm" | "heroku" | "hugo" | "ip" | "kind" | "kubectl" | "kustomize"
        | "make" | "mc" | "minikube" | "mongosh" | "mysql" | "mvn" | "ng" | "npm" | "nvm"
        | "nx" | "openssl" | "pip" | "pipenv" | "pnpm" | "poetry" | "podman" | "psql"
        | "pulumi" | "pyenv" | "python" | "rake" | "rbenv" | "redis-cli" | "rustup"
        | "serverless" | "skaffold" | "sls" | "sst" | "swift" | "systemctl" | "terraform"
        | "tmux" | "turbo" | "ufw" | "vault" | "vercel" | "volta" | "wp" | "yarn" => 2,
        "bun run"
        | "bun x"
        | "cargo add"
        | "cargo run"
        | "consul kv"
        | "deno task"
        | "docker builder"
        | "docker compose"
        | "docker container"
        | "docker image"
        | "docker network"
        | "docker volume"
        | "eksctl create"
        | "git config"
        | "git remote"
        | "git stash"
        | "ip addr"
        | "ip link"
        | "ip netns"
        | "ip route"
        | "kind create"
        | "kubectl kustomize"
        | "kubectl rollout"
        | "mc admin"
        | "npm exec"
        | "npm init"
        | "npm run"
        | "npm view"
        | "openssl req"
        | "openssl x509"
        | "pnpm dlx"
        | "pnpm exec"
        | "pnpm run"
        | "podman container"
        | "podman image"
        | "pulumi stack"
        | "terraform workspace"
        | "vault auth"
        | "vault kv"
        | "yarn dlx"
        | "yarn run" => 3,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::prefix;

    fn tokens(input: &[&str]) -> Vec<String> {
        input.iter().map(|token| (*token).to_owned()).collect()
    }

    #[test]
    fn uses_the_longest_known_prefix() {
        assert_eq!(
            prefix(&tokens(&["npm", "run", "dev", "--", "--port", "3000"])),
            tokens(&["npm", "run", "dev"])
        );
        assert_eq!(
            prefix(&tokens(&["docker", "compose", "up", "-d"])),
            tokens(&["docker", "compose", "up"])
        );
    }

    #[test]
    fn unknown_commands_default_to_the_executable() {
        assert_eq!(
            prefix(&tokens(&["custom-tool", "deploy", "prod"])),
            tokens(&["custom-tool"])
        );
    }
}
