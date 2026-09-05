# peq 1.0 — plano de entrega

Este documento registra o plano vigente para transformar o `peq` em um equalizador
estéreo para Linux/PipeWire, confortável para uso diário. Ele é a referência de
escopo, dependências e critérios de aceite da versão 1.0.

## Como ler este plano

As tarefas são identificadas por fase e número (`A01` a `F08`). Uma tarefa só fica
concluída quando sua entrega e seu critério de aceite forem verificados no código ou
no ensaio correspondente. Dependências são obrigatórias: consumidores de contratos
compartilhados entram depois que esses contratos estiverem estáveis.

O plano atual substitui a restrição histórica contra DSP próprio, os slots tipados
fixos e o fallback de reinício global.
Essas premissas permanecem preservadas em [`prompt.md`](../prompt.md) e
[`NOTES.md`](../NOTES.md) para registrar a evolução do projeto; não são requisitos
vigentes quando contradizem este documento.

## Objetivo e limites da versão 1.0

O produto mantém o executável `peq`, escrito em Rust com Ratatui e Crossterm. Ao ser
executado em um terminal interativo, `peq` abre a TUI; com saída redirecionada,
continua produzindo texto. Os comandos existentes são preservados, com `peq tui` e
`peq apply <nome>` como formas explícitas para evitar conflitos entre nomes de
presets e subcomandos.

O alvo prioritário é Arch Linux com PipeWire/WirePlumber. Outras distribuições terão
instruções de instalação, sem promessa inicial de paridade. O editor aceita até 20
bandas em qualquer combinação de peaking, lowshelf e highshelf, cada uma com
ativação individual. O preamp permanece manual; níveis, clipping e margem sugerida
são exibidos sem alterar o ganho automaticamente.

Ficam fora da 1.0: interface gráfica desktop, microfone, convolução/FIR, hospedagem
de plugins e serviços em nuvem. A biblioteca pessoal de presets é preservada; o
pacote distribuível inclui apenas presets sintéticos de demonstração, salvo quando a
proveniência de curvas externas permitir sua redistribuição.

## Arquitetura vigente

O `peq` terá um daemon próprio, controlado pela CLI e pela TUI, usando `pw_filter` e
processamento DSP em Rust. CLI e TUI compartilham serviços de aplicação e não
duplicam regras de negócio.

```text
CLI ─┐
     ├── socket Unix ── daemon peq ── estado e controle
TUI ─┘                         │
                               ▼
Aplicações ── sink peq estável ── DSP estéreo ── saída selecionada
```

O filtro é duplex, com links explícitos. A integração inicial precisa comprovar que
duas aplicações reproduzem simultaneamente pelo sink `peq`. Chamadas nativas do
PipeWire ficam encapsuladas em um módulo pequeno, com gerenciamento de recursos e
comentários sobre cada operação `unsafe`.

Os coeficientes são calculados fora do callback. O callback trabalha com memória
previamente alocada, sem locks, arquivos, logging ou outras operações de entrada e
saída. Bancos de filtros são trocados por revisão e passam por uma transição linear
de 20 ms; rajadas de edição são agrupadas antes do envio. O áudio e o gráfico usam o
sample rate efetivo. O caminho normal nunca reinicia globalmente o PipeWire nem
muda a saída sem ação explícita.

CLI e TUI usam os mesmos contratos centrais:

| Contrato | Responsabilidade |
| --- | --- |
| `PresetDocument` | Preset versionado, identidade estável, metadados, preamp e bandas |
| `PresetStore` | Leitura, validação, gravação atômica, importação e gerenciamento |
| `AppliedSnapshot` | Conteúdo confirmado pelo motor, revisão e bypass |
| `EngineStatus` | Conexão, saída, sample rate, revisão aplicada e métricas |
| `EngineClient` | Aplicar, consultar, observar eventos, bypass e selecionar saída |
| `AppState` | Seleção, rascunho, histórico de edição e estado das telas |

O protocolo local é JSON Lines versionado sobre socket Unix restrito ao usuário. Cada
requisição tem identificador, revisão e timeout; uma resposta de sucesso só é
emitida depois da confirmação do motor.

Salvar altera a biblioteca. Aplicar confirma e persiste o conteúdo que será
restaurado futuramente. Ouvir ao vivo aplica uma prévia temporária; A/B alterna entre
o conteúdo aplicado e o rascunho; bypass suspende a equalização preservando o
conteúdo aplicado. A prévia tem sessão exclusiva e expiração. Ao desconectar o
editor, o daemon restaura o último conteúdo confirmado. Uma aplicação externa mais
recente sempre vence uma TUI antiga.

## Direção da interface

A TUI usa fundo grafite, destaque ciano para foco, curvas em cores distintas e
atenção em âmbar. Todo estado também aparece por texto e símbolo, de modo que cor ou
fonte especial não sejam necessários. Em telas largas, biblioteca e editor ficam
visíveis; em 80×24, os painéis alternam; abaixo disso, a interface orienta o usuário
sem descartar o rascunho.

Atalhos padrão: `Tab` alterna painéis; setas ou `hjkl` navegam; `Enter` edita; `/`
busca; `Ctrl-S` salva; `a` aplica quando não há campo em edição; `u` desfaz; `Ctrl-R`
refaz; `?` abre ajuda. Enquanto um campo de texto está em edição, os comandos do
campo têm precedência.

## Limites e segurança dos presets

Frequências ficam entre 20 Hz e 20 kHz e sempre abaixo de Nyquist; Q fica entre
0,05 e 50; ganho entre −24 e +24 dB; preamp entre −60 e +12 dB. Números não finitos,
nomes que escapem do diretório de presets e mais de 20 bandas são rejeitados com
diagnóstico, sem correção silenciosa.

Arquivos TOML antigos são lidos sem regravação implícita. Gravações usam caminho XDG,
arquivo temporário, flush/sync quando aplicável, backup e rename atômico; falhas
preservam o último arquivo válido e edições concorrentes são detectadas. Presets
inválidos não impedem listar os demais. Remoção é recuperável. Importações parciais
exigem aceite explícito e arquivos irreconhecíveis nunca resultam em preset vazio
apresentado como sucesso.

## Tarefas e critérios de aceite

### A — Fundação e verificação inicial

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| A01 | Registrar este plano, as decisões arquiteturais e a evolução histórica; distinguir requisitos atuais das instruções arquivadas. | — | concluída |
| A02 | Separar biblioteca, aplicação e adaptadores; CLI/TUI deixam de concentrar regras de negócio. | A01 | concluída |
| A03 | Criar CI de compilação, formatação, Clippy e testes; permitir testar o núcleo sem dependências nativas de áudio. | A02 | concluída |
| A04 | Implementar motor simulado com sucesso, atraso, desconexão e falha; clientes podem ser desenvolvidos sem PipeWire. | A02 | pendente |
| A05 | Criar sessão PipeWire de teste com socket e diretórios próprios, sem dispositivos físicos; execução não altera a sessão normal. | A03 | pendente |

### B — Presets, validação e persistência

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| B01 | Introduzir documento versionado, identidade estável e `enabled` por banda; ler TOML antigo sem regravá-lo implicitamente. | A02 | concluída |
| B02 | Centralizar validação de nomes, números e limites; rejeitar não finitos, caminhos indevidos e mais de 20 bandas. | B01 | pendente |
| B03 | Implementar XDG, gravação atômica, backup e edição concorrente; falhas preservam o último arquivo válido. | B02 | pendente |
| B04 | Implementar criação, duplicação, renomeação, remoção recuperável e favoritos; presets inválidos não impedem listar os demais. | B03 | pendente |
| B05 | Fortalecer importação AutoEQ/SquigLink TXT, inclusive arquivos sem extensão; diagnósticos identificam linha e motivo. | B02 | pendente |
| B06 | Importar o JSON legado do corpus, com mapeamento explícito dos tipos conhecidos; comparar semanticamente com o TXT correspondente. | B02 | pendente |
| B07 | Implementar prévia e importação em lote, com relatório de colisões e perdas; nunca sobrescrever ou truncar silenciosamente. | B03, B05, B06 | pendente |
| B08 | Exportar TOML e TXT compatível; exportação seguida de importação preserva curva e preamp. | B05, B07 | pendente |

### C — Motor de áudio contínuo

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| C01 | Tornar sample rate explícito no cálculo de coeficientes e resposta; validar 44,1/48/96/192 kHz. | B02 | pendente |
| C02 | Processar blocos estéreo com estados independentes por canal; validar impulso, seno, silêncio e bypass. | C01 | pendente |
| C03 | Encapsular `pw_filter` e comprovar passagem estéreo com links explícitos na sessão isolada; duas aplicações tocam simultaneamente. | A05 | pendente |
| C04 | Conectar controle e processamento por fila limitada e bancos pré-alocados; atualizações entram por revisão sem bloquear o callback. | C02, C03 | pendente |
| C05 | Implementar transições de preset e bypass em 20 ms; rajadas convergem ao último estado sem recriar o sink. | C04 | pendente |
| C06 | Medir pico, RMS e clipping de entrada/saída; publicar métricas sem logging ou alocações no processamento. | C04 | pendente |
| C07 | Descobrir saídas e acompanhar adição/remoção por eventos; identificar dispositivos além de IDs temporários. | C03 | pendente |
| C08 | Implementar seleção de saída e manter links pertencentes ao `peq`; impedir ciclos e preservar links alheios. | C07 | pendente |
| C09 | Tratar perda do servidor, reconexão e mudança de sample rate; recuperar estado confirmado sem restaurar prévia abandonada. | C05, C08 | pendente |

Marco obrigatório: C03 passa antes de integrar o motor completo. Um backend que
recrie o sink a cada edição ou reinicie o PipeWire não atende à versão 1.0.

### D — Daemon, estado e CLI

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| D01 | Implementar servidor e cliente IPC versionados, assinaturas de eventos e timeouts; testar clientes lentos e mensagens inválidas. | A04, B01 | pendente |
| D02 | Implementar `daemon run/start/stop/status`, instância única e encerramento controlado; fechar a TUI não encerra o áudio. | D01, C09 | pendente |
| D03 | Unificar aplicação e bypass com confirmação do motor e persistência do snapshot; falhas não são sucesso. | D02, B03 | pendente |
| D04 | Implementar sessões temporárias de prévia/A-B com expiração e revisão; desconexão não deixa edição temporária ativa. | D03 | pendente |
| D05 | Associar dispositivos a presets por padrões `match`; automação inicialmente desligada e conflitos apresentados ao usuário. | D03, C08, B04 | pendente |
| D06 | Migrar comandos e implementar entrada TTY/TUI; remover restart global do fluxo público de aplicação. | D03, B04 | pendente |
| D07 | Expor gerenciamento, importação e exportação pela CLI; automações recebem códigos de saída e sobrescrita explícita. | D06, B08 | pendente |
| D08 | Implementar `doctor` e status confiável com `serde_json`; preservar Waybar e oferecer estado detalhado separado. | D03, C08 | pendente |

Se a automação por dispositivo for habilitada, escolha manual suspende a seleção
automática até a próxima troca de saída ou reativação explícita. Padrões ambíguos
nunca escolhem um preset arbitrariamente.

### E — TUI completa

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| E01 | Criar estado da aplicação e ações assíncronas; falhas preservam seleção, rascunho e histórico. | A04, B01, D01 | pendente |
| E02 | Proteger raw mode e tela alternativa com guard; verificar saída, erro, Ctrl-C e panic com unwinding. | A02 | pendente |
| E03 | Implementar layout adaptável e tema; validar painéis largos, compacto e mensagens essenciais sem cortes. | E01, E02 | pendente |
| E04 | Criar biblioteca com busca, favoritos e gerenciamento; trocar de preset protege alterações não salvas. | E03, B04 | pendente |
| E05 | Implementar tabela com foco e rolagem automática; todas as bandas acessíveis e visíveis ao selecionar. | E03 | pendente |
| E06 | Adicionar edição numérica direta e ajustes fino/grosso; erros ficam junto ao campo sem encerrar. | E05, B02 | pendente |
| E07 | Adicionar, duplicar, remover, reordenar, habilitar e trocar tipos; gráfico acompanha operações. | E06 | pendente |
| E08 | Implementar undo/redo e restauração de campo/banda/preset; conteúdo salvo remove indicador de alteração. | E07 | pendente |
| E09 | Criar gráfico com eixos, linha de 0 dB, legenda, cursor e banda selecionada; compartilhar cálculos com áudio. | E03, C01 | pendente |
| E10 | Implementar salvar, salvar como, aplicar e descartar com estados distintos; aplicar não finge salvar. | E04, D03 | pendente |
| E11 | Adicionar prévia ao vivo e A/B visual/sonora; exibir perda de posse quando outra CLI altera o estado. | E09, E10, D04 | pendente |
| E12 | Criar importação/exportação com prévia, diagnósticos e colisões. | E04, B07, B08 | pendente |
| E13 | Criar tela de saídas e associações; mostrar desconexão e automação ativa/inativa. | E03, D05 | pendente |
| E14 | Integrar níveis, clipping, conexão e sample rate; ausência de áudio é estado compreensível. | E03, C06, D08 | pendente |
| E15 | Implementar ajuda pesquisável, atalhos contextuais, alto contraste e ASCII; navegação completa por teclado. | E03, E10 | pendente |
| E16 | Implementar primeira execução guiada e recuperação de rascunho; diagnóstico, importação e saída dentro da TUI. | E10, E12, E13, D08 | pendente |

### F — Qualidade, instalação e entrega

| ID | Entrega e critério de aceite | Depende de | Status |
| --- | --- | --- | --- |
| F01 | Testar biblioteca, IPC e estado: concorrência, timeout, falha de gravação, bypass e revisão obsoleta. | B03, D04, D07 | pendente |
| F02 | Criar snapshots TUI em 80×24, 120×40 e 160×48, com modais, erros e nomes longos. | E03–E15 | pendente |
| F03 | Criar testes PTY para editar, salvar, aplicar e sair; confirmar terminal restaurado e rascunho preservado. | E16 | pendente |
| F04 | Implementar `init` e migração com backups; preservar presets antigos e detectar legado sem reiniciar serviços globais. | B03, D06 | pendente |
| F05 | Preparar instalação e pacote Arch, serviço systemd de usuário opcional e completions; validar ambiente limpo. | D02, D07, F04 | pendente |
| F06 | Atualizar README, guia, troubleshooting e imagens reais da TUI; registrar licença e proveniência dos materiais. | E16, F05 | pendente |
| F07 | Automatizar artefatos de release, checksums e validação do pacote; release local sem publicação automática. | A03, F05 | pendente |
| F08 | Executar ensaio final de áudio, desempenho e uso; entregar relatório com resultados e limitações observadas. | C09, F01–F07 | pendente |

## Ordem de execução

O trabalho avança respeitando as dependências em três frentes: fundação, validação e
persistência; prova do motor e integração de áudio; e TUI desenvolvida contra o
motor simulado. O coordenador revisa cada entrega, integra contratos antes de
liberar consumidores e devolve bugs à tarefa responsável.

## Definição de pronto da versão 1.0

A versão só fica pronta quando troca de preset, edição ao vivo e bypass preservam o
PipeWire e o sink estável; CLI, TUI e daemon concordam sobre o conteúdo aplicado; dois
aplicativos reproduzem simultaneamente; perda de saída ou reinício do daemon tem
recuperação previsível; resposta calculada coincide com áudio processado; o callback
não aloca, bloqueia nem faz entrada/saída; importações, gravações e concorrência não
causam perda silenciosa; o fluxo principal funciona por teclado em 80×24; comandos
preservados e Waybar continuam funcionando; e instalação, migração, testes e pacote
passam.

Devem ser medidos na máquina de referência: aplicação confirmada até 50 ms no P95
com daemon ativo, edição ao vivo estabilizada até 100 ms e ensaio de 30 minutos com
mudanças repetidas sem interrupções atribuíveis ao `peq`. São critérios de avaliação,
não resultados presumidos.
