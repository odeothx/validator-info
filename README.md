# Creditcoin 벨리데이터 정보 도구 (validator-info)

Creditcoin 메인넷의 벨리데이터 정보를 실시간으로 조회하고, 스테이킹 보상을 최적화할 수 있는 추천 기능을 제공하는 Rust 기반 도구입니다.

## 주요 기능

- **벨리데이터 상태 조회**: 활성 벨리데이터의 수수료, 전체 스테이킹 양, 본인 스테이킹 양, 노미네이터 수, 에라 포인트(실시간 활동 점수)를 표시합니다.
- **내 스테이킹 확인**: `my_accounts.txt`에 주소를 등록하면, 내가 해당 벨리데이터에 실제로 스테이킹 중인 금액(**My Stake**)을 별도로 계산하여 보여줍니다.
- **보상 최적화 추천**: `--recommend` 옵션을 통해 현재 가장 보상 효율이 높은 벨리데이터를 추천받을 수 있습니다.
  - **최근 7일간 Payout 이력이 없는 노드는 자동 제외**하여 안정성을 보장합니다.
  - **보상 점수(Score)**: `(1 - 수수료%) / 전체 스테이킹 양` 공식을 사용하여, 내가 받을 파이가 큰 노드를 우선 추천합니다.
- **완벽한 레이아웃**: 이모지(🌐) 등 특수 문자가 포함된 이름도 터미널에서 흐트러짐 없이 정렬되어 출력됩니다.

## 시작하기

### 1. 사전 준비
- [Rust](https://www.rust-lang.org/tools/install)가 설치되어 있어야 합니다.

### 2. 계정 설정
본인의 Creditcoin 계정(Stash 또는 Controller) 주소를 `my_accounts.txt` 파일에 작성해 주세요. (기본 파일명은 `my_accounts.txt`이며, `--accounts` 옵션으로 변경 가능합니다.)

**형식:** `<별칭> <주소>` (중간 공백)
```text
MyWallet1 5Ds...
MyWallet2 5G9...
```
*참고: 예시 파일인 `example_accounts.txt`를 참고하여 작성하실 수 있습니다.*

### 3. 실행 방법

**전체 벨리데이터 목록 조회:**
```bash
cargo run --release
```

**내가 스테이킹 중인 노드만 필터링:**
```bash
cargo run --release -- --mine
```

**미지급 보상이 있는 벨리데이터 확인 (NEW!):**
```bash
cargo run --release -- --unpaid
```
내가 노미네이팅 중인 벨리데이터 중 아직 Payout이 되지 않은 Era가 있는 노드를 찾아줍니다.

**최적의 보상 벨리데이터 추천 (강력 추천):**
```bash
cargo run --release -- --recommend
```

**RPC 서버 및 지갑 정보 파일 변경:**
```bash
cargo run --release -- --url "wss://rpc.creditcoin.network" --accounts "other_accounts.txt"
```
- `--url`, `-u`: 접속할 RPC 노드 URL을 지정합니다. (기본값: `wss://mainnet3.creditcoin.network`)
- `--accounts`, `-a`: 지갑 정보가 담긴 텍스트 파일을 지정합니다. (기본값: `my_accounts.txt`)

## 컬럼 설명

| 컬럼명 | 설명 |
| :--- | :--- |
| **Address** | 벨리데이터의 지갑 주소 |
| **Name** | 벨리데이터 이름 (Identity 정보 포함) |
| **Comm** | 벨리데이터 수수료 (낮을수록 보상이 많음) |
| **Total (CTC)** | 해당 노드에 예치된 전체 스테이킹 양 |
| **Own (CTC)** | 벨리데이터 운영자가 직접 예치한 양 |
| **My (CTC)** | **내 계정들이 해당 노드에 실제로 스테이킹 중인 양** |
| **Noms** | 현재 해당 노드를 선택한 전체 노미네이터 수 |
| **Points** | 현재 에라(Era)에서 획득한 활동 점수 (실시간 성능 지표) |
| **Score** | 보상 효율 점수 (높을수록 추천) |
| **Last Payout** | 가장 최근 보상 정산이 이루어진 날짜 |

## 주의사항
- 본 문서 및 도구에서 제공하는 추천 점수는 네트워크 데이터를 기반으로 한 참고용이며, 최종 투자 결정은 본인의 책임하에 진행해 주세요.
- `my_accounts.txt` 파일은 개인적인 주소가 포함되므로 Git에 커밋되지 않도록 `.gitignore`에 자동 등록되어 있습니다.
