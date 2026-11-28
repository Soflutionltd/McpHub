//! Bibliothèque intégrée de services connus, en sommeil tant qu'on ne les connecte pas.
//!
//! Un service du catalogue n'occupe rien : ni processus, ni jeton, ni outil dans le contexte de
//! l'agent. « Connecte mon Notion » suffit : l'agent appelle `connect`, McpHub ajoute la recette
//! vérifiée à sa config, ouvre la page d'autorisation du service en arrière-plan, et le serveur
//! s'active dès que la personne a cliqué « Autoriser ». Le jeton va directement au Trousseau :
//! l'agent ne le voit jamais.
//!
//! Toutes les recettes sont des serveurs distants officiels (aucun programme local lancé), dont
//! l'adresse a été vérifiée auprès de l'éditeur.

use serde_json::{json, Value};

/// Connexion d'un service du catalogue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Access {
    /// OAuth en un clic (enregistrement automatique du client, page d'autorisation).
    OAuth,
    /// Jeton d'API dans un en-tête : code d'appareil ou lien pré-rempli puis collage.
    Token { var: &'static str },
    /// Aucun compte.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Service {
    /// Nom donné au serveur dans la config.
    pub name: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// Mots de recherche (français et anglais), en minuscules.
    pub keywords: &'static [&'static str],
    pub url: &'static str,
    pub access: Access,
}

pub const SERVICES: &[Service] = &[
    Service {
        name: "linear",
        label: "Linear",
        description: "Issues, projects and cycles in Linear",
        keywords: &[
            "linear", "issue", "ticket", "tache", "projet", "sprint", "cycle",
        ],
        url: "https://mcp.linear.app/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "notion",
        label: "Notion",
        description: "Pages and databases in Notion",
        keywords: &[
            "notion",
            "page",
            "note",
            "wiki",
            "documentation",
            "base de donnees",
            "database",
        ],
        url: "https://mcp.notion.com/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "sentry",
        label: "Sentry",
        description: "Errors, issues and releases in Sentry",
        keywords: &[
            "sentry",
            "erreur",
            "error",
            "crash",
            "exception",
            "monitoring",
            "bug",
        ],
        url: "https://mcp.sentry.dev/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "vercel",
        label: "Vercel",
        description: "Deployments, projects and logs on Vercel",
        keywords: &[
            "vercel",
            "deploiement",
            "deploy",
            "hebergement",
            "hosting",
            "next",
        ],
        url: "https://mcp.vercel.com",
        access: Access::OAuth,
    },
    Service {
        name: "figma",
        label: "Figma",
        description: "Designs, frames and components in Figma",
        keywords: &[
            "figma",
            "design",
            "maquette",
            "mockup",
            "composant",
            "component",
            "ui",
        ],
        url: "https://mcp.figma.com/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "supabase",
        label: "Supabase",
        description: "Supabase projects, database, edge functions and storage",
        keywords: &[
            "supabase",
            "postgres",
            "sql",
            "base de donnees",
            "database",
            "storage",
            "edge",
        ],
        url: "https://mcp.supabase.com/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "cloudflare",
        label: "Cloudflare",
        description: "Workers, Pages, D1, KV, R2 and DNS on Cloudflare (Code Mode)",
        keywords: &[
            "cloudflare",
            "workers",
            "pages",
            "dns",
            "d1",
            "kv",
            "r2",
            "deploiement",
            "deploy",
        ],
        url: "https://mcp.cloudflare.com/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "stripe",
        label: "Stripe",
        description: "Customers, payments, subscriptions and invoices in Stripe",
        keywords: &[
            "stripe",
            "paiement",
            "payment",
            "abonnement",
            "subscription",
            "facture",
            "invoice",
            "client",
        ],
        url: "https://mcp.stripe.com",
        access: Access::OAuth,
    },
    Service {
        name: "atlassian",
        label: "Atlassian (Jira, Confluence)",
        description: "Jira issues and Confluence pages",
        keywords: &["atlassian", "jira", "confluence", "ticket", "issue", "wiki"],
        url: "https://mcp.atlassian.com/v1/mcp",
        access: Access::OAuth,
    },
    Service {
        name: "github",
        label: "GitHub",
        description: "Repositories, issues, pull requests and Actions on GitHub",
        keywords: &[
            "github",
            "git",
            "depot",
            "repository",
            "repo",
            "pull request",
            "issue",
            "actions",
        ],
        url: "https://api.githubcopilot.com/mcp/",
        access: Access::Token {
            var: "GITHUB_PERSONAL_ACCESS_TOKEN",
        },
    },
    Service {
        name: "context7",
        label: "Context7",
        description: "Up-to-date documentation and code examples for libraries",
        keywords: &[
            "context7",
            "documentation",
            "doc",
            "librairie",
            "library",
            "api",
            "exemple",
        ],
        url: "https://mcp.context7.com/mcp",
        access: Access::Open,
    },
    Service {
        name: "svelte",
        label: "Svelte",
        description: "Svelte and SvelteKit documentation and code checks",
        keywords: &[
            "svelte",
            "sveltekit",
            "documentation",
            "composant",
            "component",
        ],
        url: "https://mcp.svelte.dev/mcp",
        access: Access::Open,
    },
];

pub fn find(name: &str) -> Option<&'static Service> {
    SERVICES
        .iter()
        .find(|s| s.name.eq_ignore_ascii_case(name.trim()))
}

/// Entrée de config du service (ce que `connect` écrit).
pub fn config_entry(service: &Service) -> Value {
    let mut entry = json!({
        "url": service.url,
        "catalog": { "id": service.name, "at": crate::audit::iso_utc_now() },
    });
    match service.access {
        Access::OAuth => entry["auth"] = json!("oauth"),
        Access::Token { var } => {
            entry["auth"] = json!("none");
            entry["headers"] = json!({ "Authorization": format!("Bearer ${{{}}}", var) });
        }
        Access::Open => entry["auth"] = json!("none"),
    }
    entry
}

/// Serveur déjà présent dans la config : même nom (sans casse) ou même adresse.
pub fn installed_as(service: &Service, configured: &[(String, Option<String>)]) -> Option<String> {
    let same_url = |u: &str| {
        u.trim_end_matches('/')
            .eq_ignore_ascii_case(service.url.trim_end_matches('/'))
    };
    configured
        .iter()
        .find(|(name, url)| {
            name.eq_ignore_ascii_case(service.name) || url.as_deref().is_some_and(same_url)
        })
        .map(|(name, _)| name.clone())
}

fn normalize(text: &str) -> String {
    text.to_lowercase()
        .chars()
        .map(|c| match c {
            'à' | 'â' | 'ä' => 'a',
            'é' | 'è' | 'ê' | 'ë' => 'e',
            'î' | 'ï' => 'i',
            'ô' | 'ö' => 'o',
            'ù' | 'û' | 'ü' => 'u',
            'ç' => 'c',
            c => c,
        })
        .collect()
}

/// Score d'un service pour une recherche : nom exact, puis mots-clés trouvés dans la demande.
fn score(service: &Service, query: &str) -> usize {
    let q = normalize(query);
    let words: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 1)
        .collect();
    let mut points = 0;
    if words.contains(&service.name) {
        points += 10;
    }
    for k in service.keywords {
        if k.contains(' ') {
            if q.contains(k) {
                points += 2;
            }
        } else if words.contains(k) {
            points += 2;
        }
    }
    points
}

/// Services pas encore connectés qui correspondent à une recherche d'outils (3 au plus).
pub fn suggestions(query: &str, configured: &[(String, Option<String>)]) -> Vec<&'static Service> {
    let mut found: Vec<(usize, &'static Service)> = SERVICES
        .iter()
        .filter(|s| installed_as(s, configured).is_none())
        .map(|s| (score(s, query), s))
        // Le nom du service, ou au moins deux mots-clés : un mot courant seul (« page »,
        // « documentation ») ne suffit pas à proposer un service.
        .filter(|(points, _)| *points >= 4)
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(b.1.name)));
    found.into_iter().take(3).map(|(_, s)| s).collect()
}

fn access_label(access: Access) -> &'static str {
    match access {
        Access::OAuth => "oauth",
        Access::Token { .. } => "token",
        Access::Open => "open",
    }
}

/// Ligne courte pour l'agent : de quoi proposer la connexion, sans aucun outil du service.
pub fn suggestion_json(service: &Service) -> Value {
    json!({
        "name": service.name,
        "label": service.label,
        "description": service.description,
        "connect": format!("mcphub_call server=mcphub tool=connect arguments={{\"name\": \"{}\"}}", service.name),
    })
}

/// Le catalogue et l'état de chaque service, pour le tableau de bord et `catalog_list`.
pub fn list_json(configured: &[(String, Option<String>)]) -> Value {
    json!(SERVICES
        .iter()
        .map(|s| {
            let installed = installed_as(s, configured);
            json!({
                "name": s.name,
                "label": s.label,
                "description": s.description,
                "url": s.url,
                "access": access_label(s.access),
                "installed": installed.is_some(),
                "server": installed,
            })
        })
        .collect::<Vec<_>>())
}

/// Outils internes du catalogue : `connect` modifie la config, les autres lisent.
pub fn is_catalog_tool(tool: &str) -> bool {
    matches!(tool, "catalog_list" | "connect" | "ready")
}

pub fn is_catalog_write_tool(tool: &str) -> bool {
    tool == "connect"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configured(items: &[(&str, Option<&str>)]) -> Vec<(String, Option<String>)> {
        items
            .iter()
            .map(|(n, u)| (n.to_string(), u.map(String::from)))
            .collect()
    }

    #[test]
    fn recettes_valides() {
        let mut names = std::collections::BTreeSet::new();
        for s in SERVICES {
            assert!(names.insert(s.name), "nom en double : {}", s.name);
            assert!(s.url.starts_with("https://"), "{}", s.name);
            assert!(crate::oauth::secure_url(s.url), "{}", s.name);
            assert!(
                s.keywords.iter().all(|k| *k == normalize(k)),
                "mot-cle non normalise : {}",
                s.name
            );
            // Même analyse que le démon au chargement de la config.
            let entry = config_entry(s);
            crate::config::parse_server_entry(s.name, &entry, None, &|v: &str| Ok(v.to_string()))
                .unwrap_or_else(|e| panic!("{} : {}", s.name, e));
        }
        assert_eq!(SERVICES.len(), 12);
    }

    #[test]
    fn entree_de_config_selon_la_connexion() {
        assert_eq!(config_entry(find("notion").unwrap())["auth"], "oauth");
        let gh = config_entry(find("GitHub").unwrap());
        assert_eq!(gh["auth"], "none");
        assert_eq!(
            gh["headers"]["Authorization"],
            "Bearer ${GITHUB_PERSONAL_ACCESS_TOKEN}"
        );
        assert_eq!(config_entry(find("context7").unwrap())["auth"], "none");
        assert_eq!(gh["catalog"]["id"], "github");
        assert!(find("inconnu").is_none());
    }

    #[test]
    fn service_deja_present() {
        let mine = configured(&[
            ("Supabase", None),
            ("mon-sentry", Some("https://mcp.sentry.dev/mcp/")),
        ]);
        assert_eq!(
            installed_as(find("supabase").unwrap(), &mine).as_deref(),
            Some("Supabase")
        );
        assert_eq!(
            installed_as(find("sentry").unwrap(), &mine).as_deref(),
            Some("mon-sentry")
        );
        assert_eq!(installed_as(find("notion").unwrap(), &mine), None);
    }

    #[test]
    fn suggestions_dans_la_recherche() {
        let none = configured(&[]);
        let names = |q: &str, c: &[(String, Option<String>)]| {
            suggestions(q, c).iter().map(|s| s.name).collect::<Vec<_>>()
        };
        assert_eq!(names("créer une page Notion", &none)[0], "notion");
        assert_eq!(names("ouvrir un ticket Jira", &none)[0], "atlassian");
        assert!(names("deployer sur cloudflare workers", &none).contains(&"cloudflare"));
        // Déjà connecté : plus proposé.
        assert!(!names("page notion", &configured(&[("notion", None)])).contains(&"notion"));
        assert!(names("météo de demain", &none).is_empty());
        // Un mot courant isolé ne propose rien.
        assert!(names("snapshot of current tab page elements", &none).is_empty());
        assert!(names("x", &none).is_empty());
    }

    #[test]
    fn etat_du_catalogue() {
        let list = list_json(&configured(&[("github", None)]));
        let gh = list
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "github")
            .unwrap();
        assert_eq!(gh["installed"], true);
        assert_eq!(gh["access"], "token");
        assert!(is_catalog_write_tool("connect") && !is_catalog_write_tool("ready"));
    }
}
