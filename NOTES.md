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

## Status

Fase 0 **não passou** no item obrigatório 2. Conforme o próprio `prompt.md`: "Se a alteração
em runtime não funcionar de jeito nenhum, pare e me avise — o projeto inteiro depende disso."
Parando aqui para decisão do usuário sobre como seguir (ver mensagem de retorno com as
opções). Nenhum código do app foi escrito ainda; apenas este NOTES.md e a config de probe
(já removida).
