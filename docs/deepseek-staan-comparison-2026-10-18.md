# DeepSeek vs Staan: French news search

Tested on 18 October 2026 with the repository’s actual Rust adapters.

**Query:** `Bardella est accusé d’antisémitisme` (the request used a straight apostrophe). Both received `n=10`, `language=fr`, `country=fr`. DeepSeek received `lang:fr`; Staan received `market=fr-fr`. No site allowlist, date restriction, enrichment, or router cache was used. DeepSeek has no country-code parameter, so localization is not identical.

| Observed measure | DeepSeek | Staan |
| --- | ---: | ---: |
| Results | 10 | 10 |
| Distinct returned hosts | 5 | 8 |
| Publication dates populated | 10 | 0 |
| Search latency, comparison run | 737 ms | 1,807 ms |
| Direct Mediapart result | No | Yes, rank 7 |
| Facebook results | 0 | 2 |

DeepSeek’s first run took 5,845 ms. Its first Fetch took 6,873 ms; repeating Fetch on the same URL took 282 ms and returned 3,055 bytes of Markdown. Repeated search took 737 ms. Upstream caching may explain the faster repeats; these measurements are individual observations, not a latency benchmark.

There were **0 identical raw URLs**, but **1 common page after removing URL fragments**: the France Inter explainer, rank 4 in DeepSeek and rank 1 in Staan. DeepSeek frequently includes `#1` or tracking fragments. The raw host count treats `rtbf.be` and `beta.rtbf.be` separately; combining those leaves four DeepSeek host families.

A separate live `--full-content` run returned non-empty Markdown for **10/10 DeepSeek results** (735 ms) and **7/10 Staan results** (2,597 ms). These are different result sets; this counts supplied bodies and does not establish completeness of every extracted article. Fetch also succeeded after the pacing change (291 ms).

## What this sample suggests

Staan produced broader coverage for investigating this particular allegation: Mediapart’s own evidence article, Le Monde, Libération, 20 Minutes, Le Parisien and TF1info, alongside Radio France and Facebook. It also surfaced follow-up coverage dated 15 October in a URL. DeepSeek concentrated on Radio France, Franceinfo, RTBF and LCP, with no direct Mediapart result in its top ten. Some DeepSeek titles repeat themselves or contain navigation text such as “Publicité”.

DeepSeek is useful as a complementary source and successfully fetched a French page. For this query, Staan provided the stronger starting set for tracing the original reporting and subsequent debate. A single query does not establish general superiority. The original publisher has a subscription barrier, and neither snippets nor a successful fetch establish the truth of an allegation.

Publication timestamps are upstream metadata, not verified dates: for example, DeepSeek returned `2026-09-27T16:00:00+00:00` for an LCP article whose page says 28 September. Preserve this caveat when sorting chronologically.

## Cross-check against French sources

Mediapart’s [1 October evidence article](https://www.mediapart.fr/journal/politique/011026/ecrits-antisemites-de-jordan-bardella-les-preuves-du-mensonge) says it publishes documents and explains its authentication method. The complete article was subscriber-only in the independent browser check. [LCP’s 28 September report](https://lcp.fr/actualites/mediapart-exhume-des-ecrits-antisemites-de-jordan-bardella-qui-conteste-et-denonce-des) attributes the alleged 2013 writings to Mediapart’s investigation and records Bardella’s formal denial. [Télérama/AFP’s 29 September report](https://www.telerama.fr/debats-reportages/accuse-d-antisemitisme-par-mediapart-jordan-bardella-porte-plainte-pour-diffamation-7033146.php) reports his announced defamation complaint. These checks support describing an allegation and denial, rather than presenting a judicial conclusion.

## Returned ranking

### Deepseek

| Rank | Host | Result |
| ---: | --- | --- |
| 1 | radiofrance.fr | ["C'est faux !" : les électeurs RN de La Seyne-sur-Mer soutiennent Jordan Bardella, accusé de propos antisémites - "C'est faux !" : les électeurs RN de La Seyne-sur-Mer soutiennent Jordan Bardella, accusé de propos antisémites](https://www.radiofrance.fr/francebleu/podcasts/l-info-d-ici-ici-provence/c-est-faux-les-electeurs-rn-de-la-seyne-sur-mer-au-soutien-de-jordan-bardella-accuse-de-propos-antisemites-2508899#1) |
| 2 | radiofrance.fr | [Le RN fait bloc face aux accusations d'antisémitisme qui visent Jordan Bardella - Publicité](https://www.radiofrance.fr/franceinter/podcasts/l-info-de-france-inter/bardella-accuse-d-antisemitisme-le-rn-fait-bloc-2173264#1) |
| 3 | rtbf.be | [Jordan Bardella accusé d’écrits antisémites : un ministre israélien prend sa défense - RTBF Actus - Jordan Bardella accusé d’écrits antisémites : un ministre israélien prend sa défense](https://www.rtbf.be/article/jordan-bardella-accuse-d-ecrits-antisemites-un-ministre-israelien-prend-sa-defense-11792505#1) |
| 4 | radiofrance.fr | [Accusé d'antisémitisme, Jordan Bardella dénonce une "barbouzerie" : ce que l'on sait des révélations de Mediapart - Publicité](https://www.radiofrance.fr/franceinter/podcasts/l-info-de-france-inter/accuse-d-antisemitisme-jordan-bardella-denonce-une-barbouzerie-ce-que-l-on-sait-des-revelations-de-mediapart-5683292#1) |
| 5 | beta.rtbf.be | [Le journal français Mediapart rapporte des écrits antisémites de Bardella, le président du RN dément - RTBF Actus](https://beta.rtbf.be/article/le-journal-francais-mediapart-rapporte-des-ecrits-antisemites-de-bardella-le-president-du-rn-dement-11791808) |
| 6 | franceinfo.fr | [Accusé d'antisémitisme, Jordan Bardella balaie "une affaire dont tout le monde se moque"](https://www.franceinfo.fr/politique/jordan-bardella/accuse-d-antisemitisme-jordan-bardella-balaie-une-affaire-dont-tout-le-monde-se-moque_8229088.html#xtor=RSS-3-%5Bgeneral%5D) |
| 7 | franceinfo.fr | ["C'est soit une manipulation, soit un faux grossier", dénonce Laurent Jacobelli, porte-parole du RN après les propos antisémites attribués à Jordan Bardella - Flash info](https://www.franceinfo.fr/politique/front-national/c-est-soit-une-manipulation-soit-un-faux-grossier-denonce-laurent-jacobelli-porte-parole-du-rn-concernant-les-propos-antisemites-attribues-a-jordan-bardella_8214335.html#at_medium=5&at_campaign_group=1&at_campaign=politique&at_offre=3&at_variant=V3&at_send_date=20260929&at_recipient_id=726375-1438236820-06cc6513&at_adid=DM1388006#1) |
| 8 | franceinfo.fr | ["Antisémitisme obsessionnel" ou "barbouzerie" ? Six questions sur les propos attribués à Jordan Bardella par Mediapart - Flash info](https://www.franceinfo.fr/politique/front-national/antisemitisme-obsessionnel-ou-barbouzerie-six-questions-sur-les-propos-attribues-a-jordan-bardella-par-mediapart_8213561.html#1) |
| 9 | lcp.fr | [Face aux révélations de Mediapart, Jordan Bardella nie tout écrit antisémite et promet de "tenir bon" \| LCP - Assemblée nationale](https://lcp.fr/actualites/face-aux-revelations-de-mediapart-jordan-bardella-nie-tout-ecrit-antisemite-et-promet-de) |
| 10 | lcp.fr | [Mediapart exhume des écrits antisémites de Jordan Bardella, qui conteste et dénonce des "faux grossiers" \| LCP - Assemblée nationale](https://lcp.fr/actualites/mediapart-exhume-des-ecrits-antisemites-de-jordan-bardella-qui-conteste-et-denonce-des) |

### Staan

| Rank | Host | Result |
| ---: | --- | --- |
| 1 | radiofrance.fr | [Accusé d'antisémitisme, Jordan Bardella dénonce une " ...](https://www.radiofrance.fr/franceinter/podcasts/l-info-de-france-inter/accuse-d-antisemitisme-jordan-bardella-denonce-une-barbouzerie-ce-que-l-on-sait-des-revelations-de-mediapart-5683292) |
| 2 | lemonde.fr | [Des « brebis galeuses » aux accusations contre Jordan ...](https://www.lemonde.fr/les-decodeurs/article/2026/10/12/des-brebis-galeuses-aux-accusations-contre-jordan-bardella-un-antisemitisme-persistant-dans-le-rassemblement-national-dediabolise_6792644_4355770.html) |
| 3 | 20minutes.fr | [Bardella accusé d'antisémitisme : On a tenté de récupérer ...](https://www.20minutes.fr/high-tech/meta/4250680-20261003-bardella-accuse-antisemitisme-tente-recuperer-anciennes-conversations-messenger) |
| 4 | facebook.com | [🔴 J. Bardella accusé d'antisémitisme : "Ce sont des propos ...](https://www.facebook.com/franceinfo/videos/j-bardella-accus%C3%A9-dantis%C3%A9mitisme/1985317388807706/) |
| 5 | liberation.fr | [Jordan Bardella accusé d'antisémitisme : les preuves ...](https://www.liberation.fr/politique/jordan-bardella-accuse-dantisemitisme-les-preuves-numeriques-en-question-20261008_ONBROT7JMVDZJKBETJ5YJ2XVUE/) |
| 6 | leparisien.fr | [Jordan Bardella accusé d'antisémitisme : les Français ne le ...](https://www.leparisien.fr/elections/presidentielle/jordan-bardella-accuse-dantisemitisme-les-francais-ne-le-croient-majoritairement-pas-les-electeurs-du-rn-prets-a-lexcuser-15-10-2026-2TLNRNF735D2FFJAXELWLESAXM.php) |
| 7 | mediapart.fr | [Écrits antisémites de Jordan Bardella : les preuves du ...](https://www.mediapart.fr/journal/politique/011026/ecrits-antisemites-de-jordan-bardella-les-preuves-du-mensonge) |
| 8 | tf1info.fr | [Accusations d'antisémitisme : Jordan Bardella dément](https://www.tf1info.fr/politique/videos/video-accusations-d-antisemitisme-jordan-bardella-dement-25994-2466867.html) |
| 9 | facebook.com | [👁️ Alors que Jordan Bardella est accusé d'antisémitisme ...](https://www.facebook.com/franceinfo/videos/le-complotiste-alain-soral-formateur-de-jordan-bardella-/2664173857331046/) |
| 10 | radiofrance.fr | [Propos antisémites attribués à Jordan Bardella : l'influence ...](https://www.radiofrance.fr/franceinter/podcasts/l-info-de-france-inter/propos-antisemites-attribues-a-jordan-bardella-l-influence-d-alain-soral-sur-le-rn-en-question-2013952) |

## Reproduce

Export `DEEPSEEK_API_KEY` (or `DEEPSEEK_KEY_1`) and `STAAN_KEY_1`, then run:

```sh
cargo run -p providers --example compare_deepseek_staan -- "Bardella est accusé d'antisémitisme"
```

Append `--full-content` for a separate enrichment comparison. Credentials are never printed. Full normalized responses for the plain-search comparison are saved in [the JSON snapshot](deepseek-staan-comparison-2026-10-18.json).

The integration follows [DeepSeek’s Search & Fetch guide](https://api-docs.deepseek.com/guides/search/). Search and Fetch have independent 5-QPS limits. Each adapter uses separate 210-ms pacing gates; startup configures a 3-second search queue so brief bursts wait before fallback. Other processes sharing the same account need aggregate traffic coordination.

## Follow-up with an explicit localization IP

At the user’s request, the same query was rerun with `lang:fr` and DeepSeek’s JSON field `"localization": "86.196.134.124"`. Staan again used `market=fr-fr`. DeepSeek returned all ten of the same URLs in the same order as the unlocalized run (781 ms); Fetch succeeded (336 ms, 3,055 Markdown bytes). The repeated Staan search took 1,072 ms and again covered eight distinct hosts, including Mediapart. Its ranking changed slightly.

This shows no observed localization effect on this query’s top ten. It does not establish whether DeepSeek ignored the hint, used upstream caching, or produced the same ranking for both locations. The IP is sent as the API’s localization hint; it does not change the caller’s network egress IP. It is not configured as a server-wide default.

Reproduce with:

```sh
cargo run -p providers --example compare_deepseek_staan -- "Bardella est accusé d'antisémitisme" --localization 86.196.134.124
```

The full localized response is saved in [the localized JSON snapshot](deepseek-staan-localized-comparison-2026-10-18.json).

## Country mapping integration verification

After adding server-side `DEEPSEEK_LOCALIZATION_IPS`, the comparison was rerun without `--localization`. With `country=fr`, the adapter selected the configured France IP automatically and returned ten results (798 ms); Staan returned ten results (3,537 ms). The server and Docker Compose accept this mapping via environment configuration. Callers continue using `country` and `language`. See the README for configuration and fallback behavior.
