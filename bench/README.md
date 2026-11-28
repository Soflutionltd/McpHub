# Banc d'essai McpHub

Il mesure la même tâche, avec le même modèle, **sans McpHub** (chaque serveur MCP branché
directement, tous ses outils envoyés au modèle) puis **avec McpHub** (chemin réel d'un
utilisateur : client Node en stdio, démon McpHub derrière).

## Ce qui est mesuré

Six tâches reproductibles (`data.py`) :

1. une recherche simple (2 appels) ;
2. 20 lectures indépendantes ;
3. un enchaînement dépendant (4 appels) ;
4. une grosse réponse à filtrer (1 500 lignes de journal) ;
5. deux comptes clients aux outils homonymes ;
6. un appel long (75 s, au-delà du délai d'environ 60 s des clients MCP).

Huit faux serveurs déterministes (`fake_server.py`, 64 outils), dont quatre serveurs de « bruit »
qui représentent les outils qu'une vraie configuration charge aussi. Pour chaque exécution :
réussite vérifiée automatiquement, tokens (entrée, sortie, cache), coût, temps, tours, appels
d'outils et mémoire des processus.

Chaque exécution démarre des processus neufs. Le démon McpHub tourne isolé (dossier, port et
coffre temporaires) : la vraie installation n'est ni lue ni modifiée.

## Lancer

```bash
python3 -m venv bench/.venv && bench/.venv/bin/pip install anthropic
ANTHROPIC_API_KEY="$(op-sof read 'op://Soflution/<élément>/credential')" \
  bench/.venv/bin/python bench/run.py --reps 5 --jobs 4
bench/.venv/bin/python bench/report.py bench/results/raw-<date>.jsonl
```

Options : `--tasks 1-lookup,4-big-filter`, `--modes direct,mcphub`, `--effort medium`,
`--mcphub <binaire>`, `--client-js <client/index.js>`.

## Limites

- Les faux serveurs répondent instantanément (sauf la tâche 6) : la lenteur réelle des services
  n'est pas mesurée.
- Le mode « direct » envoie au modèle la liste complète des outils, comme un client qui branche
  chaque serveur. Les clients qui chargent les outils à la demande (recherche d'outils) ne sont
  pas représentés.
- Les deux modes utilisent le cache de prompt de l'API, comme les clients courants.
- Les résultats varient d'une exécution à l'autre : d'où les répétitions, et la publication des
  résultats bruts dans `results/`.
