# nDB vs MariaDB: một entity có thực sự "nặng" hơn một dòng SQL?

> Bài phân tích chi phí lưu trữ ở mức byte, dựa trực tiếp trên mã nguồn on-disk của nDB
> (`crates/ndb-engine/src/record.rs`, `value.rs`, `codec.rs`) và row format DYNAMIC của
> InnoDB/MariaDB. Sẵn sàng chia sẻ.

## TL;DR

- **Bản ghi nhỏ (vài byte dữ liệu):** MariaDB gọn hơn rõ rệt (~34B vs ~81B, tức ~2.4×).
- **Bản ghi lớn (vector, chuỗi sinh học, blob):** overhead cố định 48B của nDB tan biến —
  hai bên chênh nhau **dưới 1.5%**.
- **Tính cả quan hệ nhiều chiều (n-ngôi):** **nDB hiệu quả hơn**, và biên độ thắng **tỷ lệ
  thuận với số chiều của quan hệ** — từ ~hòa (nhị phân) đến **~3.4×** (arity 1500).

Tóm một câu: nDB trả một khoản "thuế cố định" để đổi lấy UUID toàn cục, schema linh hoạt,
MVCC time-travel và quan hệ n-ngôi nguyên thủy. Khoản thuế đó **đau khi bạn lưu vài byte**
(việc của MariaDB) và **biến mất khi bạn lưu kilobyte dữ liệu khoa học cùng quan hệ phức tạp**
(việc của nDB).

---

## 1. MariaDB (InnoDB) lưu một dòng thế nào

Row format DYNAMIC, trong B+tree clustered index, page 16KB:

| Thành phần | Kích thước |
|---|---|
| Record header (next-ptr, flags, heap no) | 5 B cố định |
| Mảng độ dài cột biến thiên | 1–2 B / cột var-len |
| NULL bitmap | 1 bit / cột nullable |
| `DB_TRX_ID` (MVCC) | 6 B |
| `DB_ROLL_PTR` (undo log) | 7 B |
| `DB_ROW_ID` (chỉ khi không có PK) | 6 B |
| Dữ liệu cột | đúng kích thước kiểu, **không tag, không tên cột** |

**Overhead cố định ≈ 18 B/dòng** (5 + 6 + 7) khi có PK người dùng. Tên cột & kiểu nằm ở
data dictionary; checksum tính **theo page** (amortize hàng trăm dòng); bản cũ MVCC nằm
trong undo log và bị purge — bảng chính chỉ giữ bản mới nhất.

## 2. nDB lưu một entity thế nào

Mỗi entity là một `EntityRecord` append-only (LSM-tree), envelope chung:

```text
┌─────────────┬──────────┬────────────────┬───────────── payload ─────────────┬───────┐
│ record_size │ rec_kind │ format_version │ entity_id │ type_id │ tx_assert/...  │ crc32 │
│   u32 = 4B  │  u8 = 1B │    u8 = 1B     │ UUID 16B  │ u32 4B  │ 2×u64 = 16B    │  4B   │
└─────────────┴──────────┴────────────────┴───────────────────────────────────┴───────┘
```

| Thành phần | Kích thước |
|---|---|
| Envelope (size + kind + version + CRC32) | 10 B |
| `entity_id` (UUID v7) | 16 B |
| `type_id` | 4 B |
| `tx_id_assert` + `tx_id_supersede` (MVCC bitemporal) | 16 B |
| Số property (u16) | 2 B |
| **Tổng cố định** | **48 B** |
| Mỗi property | `PropertyId` 4B + tag 1B + giá trị |

Tên property **không** lưu lặp — nДB dùng dictionary (`PropertyKeyRecord` ánh xạ `u32 ↔ tên`,
ghi một lần), giống cơ chế data dictionary của InnoDB.

## 3. So sánh trực tiếp — một bản ghi nhỏ

`{name: "Alice", age: 30, active: true}`

- **nDB:** 48 (header) + 14 (name) + 13 (age) + 6 (active) = **~81 B**
- **InnoDB:** 5 (header) + 1 (varlen) + 1 (null bitmap) + 13 (MVCC) + 14 (data) = **~34 B**

➡️ Với dòng nhỏ, **MariaDB gọn hơn ~2.4×**. Đây là sân nhà của relational, và **không** phải
use-case nДB nhắm tới.

## 4. Khi payload nuốt chửng overhead

Header nDB là **hằng số 48 B**, payload tăng tuyến tính, nên `overhead = 48 / (48 + payload)`:

| Tình huống | Tổng record | 48B chiếm |
|---|---|---|
| Entity nhỏ (3 scalar) | 81 B | **59%** 🔴 |
| Field text 2 KB | 2.105 B | **2.3%** |
| Embedding 768-d (f32) | 3.129 B | **1.5%** |
| Embedding 1536-d (OpenAI) | 6.201 B | **0.77%** |
| Chuỗi DNA 10 kbp (2-bit packed) | ~2.557 B | **1.9%** |
| Ảnh thumbnail 50 KB | ~51.257 B | **0.09%** 🟢 |

Ở vùng này, một dòng InnoDB lưu embedding 768-d cũng phải dùng `BLOB`:
**~3.098 B (InnoDB) vs 3.129 B (nDB) — chênh ~1%**. So sánh "ai nặng hơn" gần như mất nghĩa.

## 5. Điểm quyết định: quan hệ nhiều chiều

Khi payload đã hòa, chi phí thật nằm ở việc lưu **mối quan hệ**.

**nDB — một `HyperEdgeRecord` cho cả quan hệ:**

```text
Cố định = 56 B   (envelope 10 + hyperedge_id 16 + type_id 4 + 2×tx_id 16
                  + entity_arity 4 + hyperedge_arity 4 + prop_count 2)
Mỗi participant = role_id 4 + UUID 16 = 20 B
→ Quan hệ arity k = 56 + 20k B, trong MỘT record liền mạch.
```

**MariaDB — không có quan hệ n-ngôi nguyên thủy** → buộc dùng bảng membership (EAV),
mỗi participant là **một dòng** `(edge_id, role, member_id)`:

| / participant | BIGINT key | UUID key (công bằng với nDB) |
|---|---|---|
| Dòng base (header 5 + trx 13 + data) | ~36 B | ~52 B |
| Index ngược trên `member_id` | ~25 B | ~40 B |
| **Tổng / participant** | **~60 B** | **~92 B** |

➡️ **nDB: 20 B/participant — MariaDB: ~60–92 B/participant (~4.6×).**

### Ví dụ end-to-end: knowledge graph hóa học

10.000 phân tử (mỗi cái có embedding 1024-d = 4 KB + vài scalar), 5.000 phản ứng,
mỗi phản ứng nối trung bình 6 phân tử có vai trò.

| | nDB | MariaDB |
|---|---|---|
| Entities/rows (payload chi phối) | 41.8 MB | 41.5 MB |
| Quan hệ | **0.88 MB** | **~2.3 MB** |
| **Tổng** | **≈ 42.7 MB** | **≈ 43.8 MB** |

Phần entity gần như hòa; toàn bộ chênh lệch đến từ khối quan hệ (**~2.6×**).

### Arity càng cao, nDB càng thắng đậm

Một protein "chứa" 1500 nguyên tử — hyperedge arity 1500:

| | nDB | MariaDB |
|---|---|---|
| Lưu trữ | **30 KB, 1 record** | **~102 KB, 1500 dòng rải rác** |
| Đọc cả quan hệ | 1 seek tuần tự | 1500 lookup qua B+tree |

➡️ **~3.4× ít dung lượng hơn** và locality tốt hơn hẳn.

## 6. Vì sao (bản chất)

1. **Mismatch cấu trúc:** SQL chỉ có quan hệ nhị phân (FK). Quan hệ n-ngôi bị "băm" thành
   k dòng → trả phí header + MVCC + **nhân đôi qua index** cho *từng* participant. nDB gói cả
   quan hệ vào 1 record, mỗi participant chỉ là slot 20 B.
2. **Index amplification:** mỗi entry index relational lặp lại PK, scale theo số participant.
   nDB chỉ index hyperedge_id, không nhân bản từng role.
3. **Locality:** nDB đọc quan hệ arity 1500 bằng 1 record tuần tự; InnoDB nhảy 1500 lần.

## 7. Cho công bằng

- **Quan hệ nhị phân thuần (arity 2):** xấp xỉ hòa; nếu MariaDB dùng PK BIGINT thì còn nhỉnh
  hơn nДB chút. nDB chỉ thắng rõ từ **arity ≥ 3** hoặc arity biến thiên.
- **nDB cũng có index riêng** — không "miễn phí" hoàn toàn — nhưng base record của quan hệ
  vẫn là 20 B/role, không phải một dòng nặng như membership table.
- **Nếu lịch sử quan trọng:** nДB giữ bản cũ trên đĩa (đến khi compaction + retention policy
  xử lý); MariaDB purge undo log. Nhưng nếu bạn cần audit/time-travel trong MariaDB, bạn phải
  tự xây bảng history — lúc đó còn tốn hơn nДB.

## Kết luận

| Kịch bản | Ai gọn hơn |
|---|---|
| Bản ghi nhỏ, vài byte dữ liệu | **MariaDB** (~2.4×) |
| Payload lớn (vector / blob / chuỗi) | **Hòa** (chênh < 1.5%) |
| Tính cả quan hệ nhị phân | Hòa → MariaDB nhỉnh chút |
| Tính cả quan hệ n-ngôi (arity ≥ 3) | **nDB** (2.6× → 3.4×) |

nDB không "nén giỏi hơn". Nó thắng vì **mô hình dữ liệu khớp với bài toán**: relational buộc
phải đập một quan hệ n-ngôi thành nhiều dòng + index, còn nDB gói trọn nó trong một hyperedge.
Đúng dữ liệu (khoa học, vector, quan hệ nhiều chiều), khoản "thuế 48 byte" không chỉ biến mất —
nó còn được hoàn lại bằng lãi.

---

*Phân tích dựa trên mã nguồn nDB engine (format on-disk v3) và InnoDB row format DYNAMIC.
Con số là ước lượng mức byte để minh họa bậc độ lớn, không phải benchmark vi mô.*
