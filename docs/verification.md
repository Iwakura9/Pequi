# Registro de verificação

Este registro descreve resultados observados durante a implementação. Os critérios
restantes em `PLAN.md` continuam pendentes; a versão 1.0 ainda não está concluída.

## Fundação integrada

- A01: plano vigente e arquitetura registrados; `prompt.md` e `NOTES.md` preservados
  como histórico.
- A02: CLI e TUI chamam o mesmo serviço de aplicação; o binário delega à biblioteca.
  A consulta legada ao PipeWire tem prazo de 500 ms. O backend legado ainda será
  substituído pelo daemon; esta etapa não comprova processamento contínuo.
- A03: feature `native-audio` opcional e CI para as duas configurações. Testes e
  Clippy usam o lockfile. O job sem áudio não instala bibliotecas PipeWire.
- B01: documento versionado, identidade, metadados e bandas habilitáveis. Os testes
  cobrem leitura legada sem regravação, identidade e roundtrip.

Após integrar B01: 24 testes passaram com `--locked --offline
--no-default-features`; Clippy com warnings proibidos e formatação passaram.

## Ambiente e biblioteca pessoal

PipeWire e bibliotecas de desenvolvimento locais: 1.6.8. Nenhum processo PipeWire,
PipeWire Pulse ou WirePlumber estava ativo na inspeção inicial desta execução.
Nenhum comando de aplicação ou restart do backend legado foi executado.

A inspeção dos 28 arquivos TXT ou sem extensão do corpus encontrou no máximo 13
filtros ativos por arquivo; seus valores de frequência, ganho e Q respeitam os novos
limites. Alguns filtros `OFF` contêm placeholders zerados, a tratar explicitamente
no importador. O JSON legado corresponde aos tipos 0=LS, 1=HS e 3=PK do TXT associado.

Ainda não foram medidos latência P95, estabilidade durante 30 minutos ou reprodução
de duas aplicações. Esses resultados dependem do novo motor e da sessão isolada.
