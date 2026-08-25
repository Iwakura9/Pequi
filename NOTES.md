# NOTES — Fase 0

## Ambiente

- PipeWire 1.6.8-1 (Arch, pacote oficial), WirePlumber, Hyprland.
- `clang`/`libclang` não estavam instalados; instalados manualmente (`sudo pacman -S clang`)
  porque `libspa-sys`/`pipewire-sys` 0.10 usam `bindgen` no build. Sem isso nada compila.

## O que funcionou

1. **Subir um filter-chain custom via `~/.config/pipewire/pipewire.conf.d/*.conf` funciona.**
   `systemctl --user restart pipewire pipewire.socket wireplumber` carrega o config e o sink
   virtual aparece em `wpctl status` / `pw-dump` normalmente, com `media.class = Audio/Sink`.

2. **Leitura de estado via `pw-dump` funciona bem.** O node do capture-side expõe dois grupos
   de `Props` no dump: um genérico (audioconvert: volume, mute, channelmix.\*, etc.) e um
   segundo, específico do filter-chain, com todos os controles endereçados como
   `"nomedonode:Porta"` (ex.: `peq_band_01:Freq`, `peq_band_01:Gain`, e até os coeficientes
   `b0..a2` — somente leitura exceto em `bq_raw`). O `PropInfo` confirma os mesmos nomes com
   `min/max/default`. Isso bate exatamente com o que o `prompt.md` e o man page descrevem.

3. **`linear` (builtin) com controles `Mult`/`Add` existe e é a escolha certa pro preamp**
   (`new = old * Mult + Add`). Não precisou do fallback `mixer`.

4. **Tipo do biquad (`label`) não é alterável em runtime** — confirmado no man page
   (`libpipewire-module-filter-chain(7)`, seção Biquads): é fixo no config. A única exceção é
   `bq_raw`, cujos `b0..a2` são graváveis em runtime — mas isso não muda o problema abaixo,
   porque o mecanismo de escrita em si está quebrado (ver próxima seção). Mantido o layout de
   slots tipados (0=lowshelf, 1-18=peaking, 19=highshelf) conforme decidido.

## BLOQUEADOR — item 2 da Fase 0: escrita em runtime não chega ao grafo

**A escrita de Props documentada (`pw-cli s <id> Props '{ "params": [...] }'`, e o
equivalente via `pipewire-rs` `Node::set_param(ParamType::Props, ...)`) é aceita sem erro,
mas os valores nunca chegam ao filter-graph.** Confirmado de forma exaustiva, não é erro de
sintaxe:

- Testado com `pw-cli` (bin) **e** com um binário Rust mínimo usando `pipewire` 0.10 +
  `libspa` 0.10 chamando `Node::set_param` diretamente — mesmo resultado nos dois.
- Testado endereçando o node de capture (`Audio/Sink`) **e** o de playback — mesmo resultado.
- Testado com o config próprio (3 bandas) **e** com o exemplo oficial do pacote,
  `/usr/share/pipewire/filter-chain/sink-eq6.conf`, sem nenhuma modificação — mesmo resultado.
- Confirmado via leitura: depois do write, `pw-dump` mostra `Gain` inalterado E os
  coeficientes `b0/b1/b2` continuam em passthrough exato (`b0=1.0, b1=0.0, b2=0.0`) — ou seja,
  não é só a leitura que está "presa": o grafo nunca recalculou os coeficientes, prova de que
  o valor nunca chegou no `spa_filter_graph_set_props` interno.
- Logs do servidor com `PIPEWIRE_DEBUG=5` mostram a chamada chegando até
  `spa.audioconvert:audioconvert.c:parse_prop_params` (a camada de adapter/audioconvert que
  todo node `Audio/Sink` usa por baixo), que loga a chave/valor recebido e então não propaga
  adiante — sem log de erro, sem log de "unknown control port". Comparado com o código-fonte
  oficial (branch master e tag 1.6.8 do pipewire.git), o caminho esperado
  (`module-filter-chain.c` → `pw_stream_events.param_changed` → `spa_filter_graph_set_props`)
  deveria disparar independente disso; na prática, empiricamente, não dispara.

**Conclusão: no PipeWire 1.6.8 desta máquina, a troca de bandas em runtime sem reload do
módulo — a premissa central do projeto inteiro — não funciona pelo mecanismo documentado**,
nem por um caminho alternativo óbvio (endereço diferente, biquad diferente, config diferente).

## Efeito colateral dos testes

Os múltiplos `systemctl --user restart pipewire` derrubaram o EasyEffects (o processo saiu;
provavelmente crashou ao perder a conexão com o PipeWire). Precisa ser reaberto manualmente.
Nada foi feito automaticamente para "consertar" isso, conforme instrução de não mexer em
roteamento sem pedido.

## Decisão (usuário, pós-Fase 0)

Duas rodadas de pergunta:

1. Diante do bloqueador acima, o usuário escolheu **"reload por preset, com mitigação"**:
   trocar de arquitetura para regenerar o config e recarregar, aceitando o custo do clique,
   mantendo a stack decidida (módulo `filter-chain` via config, não DSP próprio).
2. Investigando a mitigação, descobri que **não existe reload parcial de módulo no PipeWire**
   (sem SIGHUP, sem `ExecReload` no unit systemd, `pw-cli load-module` só carrega no processo
   local do próprio `pw-cli`, nunca no daemon remoto — confirmado no `man pw-cli`: "It is not
   possible in PipeWire to load modules in another instance."). "Recarregar só o filter-chain"
   na prática só é possível via `systemctl --user restart pipewire` inteiro — derruba TODOS os
   sinks/sources do sistema, não só o do peq. Levei essa descoberta de volta; o usuário
   respondeu para eu decidir sozinho e entregar. Optei pelo restart completo (opção
   recomendada): mantém a stack decidida, sem reimplementar em Rust a montagem manual de nós
   (`create-node`/`create-link` na factory `filter.graph`), que teria risco equivalente de
   esbarrar no mesmo tipo de bug e exigiria muito mais código pra manter.

## Arquitetura final (diferente do prompt.md original)

- `peq <nome>` agora: carrega o preset → `chain::write_config` regenera
  `~/.config/pipewire/pipewire.conf.d/99-peq.conf` com os valores do preset já embutidos como
  valores **iniciais** dos 20 slots → `systemctl --user restart pipewire pipewire.socket
  wireplumber` → aguarda o sink `peq` reaparecer (`pw::reload` em `src/pw.rs`).
- **Não há mais escrita de Props em runtime em lugar nenhum do código.** `src/pw.rs` só lê
  (existência do node) e restart o serviço.
- Consequência direta: o requisito "abaixo de 50ms" e "uma única escrita de Props atômica"
  do `prompt.md` não se aplicam mais — não têm como se aplicar dado o bloqueador. Medido na
  prática: `peq <nome>` completo (regenerar config + restart do pipewire + confirmar que o
  sink voltou) ficou em ~200-300ms nesta máquina, bem mais rápido do que eu esperava para um
  restart completo do daemon.
- TUI: o "aplicar ao vivo com debounce de 100ms" do prompt.md original também não faz sentido
  mais (restartaria o pipewire dezenas de vezes por segundo arrastando um valor). A prévia da
  curva continua instantânea e 100% local (sem tocar o PipeWire); aplicar de fato ao sink real
  virou uma ação explícita (`a`), com debounce de 800ms só pra não empilhar restarts se a
  tecla for segurada.
- `peq off`/`peq on`: o estado de bypass agora é um arquivo-marcador local
  (`~/.local/state/peq/bypassed`), não algo lido do PipeWire — como toda escrita de gain
  passa a ser via regeneração de config, não runtime, não tem mais "fonte de verdade no
  PipeWire" pros valores de banda; só a *existência* do sink `peq` é lida do PipeWire (usado
  em `peq status` pra decidir `disconnected`).

## Verificação end-to-end (feita, não só assumida)

Com o binário `release` real: `peq init` → import de um `ParametricEQ.txt` sintético (LSC +
4 PK + HSC, com uma linha `OFF` e preamp -6dB) → `peq hd6` (fuzzy match) aplicou em ~264ms →
conferido via `pw-dump` que o node `peq` subiu com `peq_preamp:Mult=0.501187` (=10^(-6/20)) e
os `Freq`/`Gain` de cada slot batendo exatamente com o preset (incluindo os slots não usados
ficando em `Gain=0.0`, passthrough) → `peq off` zerou tudo (~278ms) → `peq on` restaurou
(~184ms) → `peq status --json` refletiu `class` corretamente em cada estado. Todos os
artefatos de teste (preset `HD6XX` sintético, config gerado) foram removidos ao final; o
PipeWire da máquina foi deixado limpo (sem sink `peq` residual, `wpctl status` normal).

## Efeito colateral residual

O EasyEffects (rodando no início da sessão) não sobreviveu aos múltiplos restarts do
PipeWire durante a Fase 0 — o processo saiu e não voltou sozinho. Não reiniciei automaticamente
(não foi pedido). Se você usa EasyEffects, `easyeffects &` (ou reabra pelo app launcher).

## Status

Projeto completo nos moldes da arquitetura revisada acima: Fases 0-6 do `prompt.md`
implementadas (`dsp`, `render`, `chain`, `preset`+import AutoEQ, `pw`, CLI completa, TUI),
com a mudança de arquitetura documentada aqui e no README. `cargo test` (15/15) e
`cargo clippy --all-targets -- -D warnings` limpos.
