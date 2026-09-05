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

## Validação, simulação e sessão isolada

A04 e B02 integrados: 35 testes passaram sem áudio nativo. O motor simulado cobre
sucesso, atraso, timeout sem mutação, desconexão, falha e conflitos de revisão.
A validação comum é usada também pelo motor simulado, inclusive Nyquist.

A05: `scripts/check-test-pipewire.sh` passou no checkout integrado com o core
`peq-a05-164554-164554`, somente um driver dummy e um sink nulo estéreo FL/FR,
sem objetos ALSA. A execução requer permissão para criar sockets locais, bloqueada
pelo sandbox padrão deste ambiente. O teste do agente verificou também retorno
de código 37 do comando filho e limpeza dos recursos próprios.

## Persistência e sample rate

B03: os testes cobrem criação, substituição por comparação de revisão, exatamente
um vencedor entre escritores concorrentes, backup recuperável, falha de backup sem
alterar o arquivo válido e listagem que mantém presets saudáveis apesar de arquivos
inválidos. Temporários privados usam rename atômico e sync; backups ficam em
`.backups/` para respeitar o limite de nomes do sistema de arquivos.

C01: coeficientes e resposta recebem o sample rate explicitamente. Os testes cobrem
44,1, 48, 96 e 192 kHz, ganho no centro, prateleiras, extremos válidos e estabilidade
dos polos. Após essa integração, os 42 testes do núcleo sem áudio nativo passaram,
assim como Clippy com warnings proibidos e formatação.

C02: `FilterBank` precomputa até 20 filtros habilitados fora do caminho de áudio;
`StereoProcessor` processa blocos planares ou intercalados com estados separados
por canal e sem alocação, lock ou I/O. Os testes cobrem impulso, ganho de seno em
Fc, silêncio, independência dos canais e bypass bit a bit. Mudanças de bypass zeram
o estado nesta etapa; a transição de 20 ms será implementada em C05. Após integrar
C02, 48 testes do núcleo passaram e Clippy permaneceu sem warnings.
