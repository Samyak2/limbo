use log::debug;

use crate::storage::pager::{Page, Pager};
use crate::storage::sqlite3_ondisk::{
    read_btree_cell, read_varint, write_varint, BTreeCell, DatabaseHeader, PageContent, PageType,
    TableInteriorCell, TableLeafCell,
};
use crate::types::{Cursor, CursorResult, OwnedRecord, OwnedValue, SeekKey, SeekOp};
use crate::Result;

use std::cell::{Ref, RefCell};
use std::pin::Pin;
use std::rc::Rc;

use super::sqlite3_ondisk::{write_varint_to_vec, IndexInteriorCell, IndexLeafCell, OverflowCell};

/*
    These are offsets of fields in the header of a b-tree page.
*/
const BTREE_HEADER_OFFSET_TYPE: usize = 0; /* type of btree page -> u8 */
const BTREE_HEADER_OFFSET_FREEBLOCK: usize = 1; /* pointer to first freeblock -> u16 */
const BTREE_HEADER_OFFSET_CELL_COUNT: usize = 3; /* number of cells in the page -> u16 */
const BTREE_HEADER_OFFSET_CELL_CONTENT: usize = 5; /* pointer to first byte of cell allocated content from top -> u16 */
const BTREE_HEADER_OFFSET_FRAGMENTED: usize = 7; /* number of fragmented bytes -> u8 */
const BTREE_HEADER_OFFSET_RIGHTMOST: usize = 8; /* if internalnode, pointer right most pointer (saved separately from cells) -> u32 */

#[derive(Debug)]
enum WriteState {
    Start,
    BalanceStart,
    BalanceGetParentPage,
    BalanceMoveUp,
    Finish,
}

struct WriteInfo {
    state: WriteState,
    new_pages: RefCell<Vec<Rc<RefCell<Page>>>>,
    scratch_cells: RefCell<Vec<&'static [u8]>>,
    rightmost_pointer: RefCell<Option<u32>>,
    page_copy: RefCell<Option<PageContent>>, // this holds the copy a of a page needed for buffer references
}

/* TODO(Pere)
** Maximum depth of an SQLite B-Tree structure. Any B-Tree deeper than
** this will be declared corrupt. This value is calculated based on a
** maximum database size of 2^31 pages a minimum fanout of 2 for a
** root-node and 3 for all other internal nodes.
**
** If a tree that appears to be taller than this is encountered, it is
** assumed that the database is corrupt.
*/
pub const BTCURSOR_MAX_DEPTH: usize = 20;

pub struct BTreeCursor {
    pager: Rc<Pager>,
    root_page: usize,
    rowid: RefCell<Option<u64>>,
    record: RefCell<Option<OwnedRecord>>,
    null_flag: bool,
    database_header: Rc<RefCell<DatabaseHeader>>,
    going_upwards: bool,
    write_info: WriteInfo,

    current_page: RefCell<i32>,
    cell_indices: RefCell<[usize; BTCURSOR_MAX_DEPTH + 1]>,
    stack: RefCell<[Option<Rc<RefCell<Page>>>; BTCURSOR_MAX_DEPTH + 1]>, // TODO(pere) stack of cell idx
                                                                         // TODO(pere) stack of pages
}

impl BTreeCursor {
    pub fn new(
        pager: Rc<Pager>,
        root_page: usize,
        database_header: Rc<RefCell<DatabaseHeader>>,
    ) -> Self {
        Self {
            pager,
            root_page,
            rowid: RefCell::new(None),
            record: RefCell::new(None),
            null_flag: false,
            database_header,
            going_upwards: false,
            write_info: WriteInfo {
                state: WriteState::Start,
                new_pages: RefCell::new(Vec::with_capacity(4)),
                scratch_cells: RefCell::new(Vec::new()),
                rightmost_pointer: RefCell::new(None),
                page_copy: RefCell::new(None),
            },
            current_page: RefCell::new(-1),
            cell_indices: RefCell::new([0; BTCURSOR_MAX_DEPTH + 1]),
            stack: RefCell::new([const { None }; BTCURSOR_MAX_DEPTH + 1]),
        }
    }

    fn is_empty_table(&mut self) -> Result<CursorResult<bool>> {
        let page = self.pager.read_page(self.root_page)?;
        let page = RefCell::borrow(&page);
        if page.is_locked() {
            return Ok(CursorResult::IO);
        }

        let page = page.contents.read().unwrap();
        let page = page.as_ref().unwrap();
        Ok(CursorResult::Ok(page.cell_count() == 0))
    }

    fn get_next_record(
        &mut self,
        predicate: Option<(SeekKey<'_>, SeekOp)>,
    ) -> Result<CursorResult<(Option<u64>, Option<OwnedRecord>)>> {
        loop {
            let mem_page_rc = self.top_from_stack();
            let cell_idx = self.current_index();

            let mem_page = RefCell::borrow(&mem_page_rc);
            debug!("current id={} cell={}", mem_page.id, cell_idx);
            if mem_page.is_locked() {
                // TODO(pere): request load page
                return Ok(CursorResult::IO);
            }
            let page = mem_page.contents.read().unwrap();
            let page = page.as_ref().unwrap();

            if cell_idx == page.cell_count() {
                // do rightmost
                let has_parent = *self.current_page.borrow() > 0;
                self.advance();
                match page.rightmost_pointer() {
                    Some(right_most_pointer) => {
                        let mem_page = self.pager.read_page(right_most_pointer as usize)?;
                        self.push_to_stack(mem_page);
                        continue;
                    }
                    None => {
                        if has_parent {
                            debug!("moving simple upwards");
                            self.going_upwards = true;
                            self.pop_from_stack();
                            continue;
                        } else {
                            return Ok(CursorResult::Ok((None, None)));
                        }
                    }
                }
            }

            if cell_idx == page.cell_count() + 1 {
                // end
                let has_parent = *self.current_page.borrow() > 0;
                if has_parent {
                    debug!("moving upwards");
                    self.going_upwards = true;
                    self.pop_from_stack();
                    continue;
                } else {
                    return Ok(CursorResult::Ok((None, None)));
                }
            }
            assert!(cell_idx < page.cell_count());

            let cell = page.cell_get(
                cell_idx,
                self.pager.clone(),
                self.max_local(page.page_type()),
                self.min_local(page.page_type()),
                self.usable_space(),
            )?;
            match &cell {
                BTreeCell::TableInteriorCell(TableInteriorCell {
                    _left_child_page,
                    _rowid,
                }) => {
                    assert!(predicate.is_none());
                    self.advance();
                    let mem_page = self.pager.read_page(*_left_child_page as usize)?;
                    self.push_to_stack(mem_page);
                    continue;
                }
                BTreeCell::TableLeafCell(TableLeafCell {
                    _rowid,
                    _payload,
                    first_overflow_page: _,
                }) => {
                    assert!(predicate.is_none());
                    self.advance();
                    let record = crate::storage::sqlite3_ondisk::read_record(_payload)?;
                    return Ok(CursorResult::Ok((Some(*_rowid), Some(record))));
                }
                BTreeCell::IndexInteriorCell(IndexInteriorCell {
                    payload,
                    left_child_page,
                    ..
                }) => {
                    if !self.going_upwards {
                        let mem_page = self.pager.read_page(*left_child_page as usize)?;
                        self.push_to_stack(mem_page);
                        continue;
                    }

                    self.going_upwards = false;
                    self.advance();

                    let record = crate::storage::sqlite3_ondisk::read_record(payload)?;
                    if predicate.is_none() {
                        let rowid = match record.values.last() {
                            Some(OwnedValue::Integer(rowid)) => *rowid as u64,
                            _ => unreachable!("index cells should have an integer rowid"),
                        };
                        return Ok(CursorResult::Ok((Some(rowid), Some(record))));
                    }

                    let (key, op) = predicate.as_ref().unwrap();
                    let SeekKey::IndexKey(index_key) = key else {
                        unreachable!("index seek key should be a record");
                    };
                    let found = match op {
                        SeekOp::GT => &record > *index_key,
                        SeekOp::GE => &record >= *index_key,
                        SeekOp::EQ => &record == *index_key,
                    };
                    if found {
                        let rowid = match record.values.last() {
                            Some(OwnedValue::Integer(rowid)) => *rowid as u64,
                            _ => unreachable!("index cells should have an integer rowid"),
                        };
                        return Ok(CursorResult::Ok((Some(rowid), Some(record))));
                    } else {
                        continue;
                    }
                }
                BTreeCell::IndexLeafCell(IndexLeafCell { payload, .. }) => {
                    self.advance();
                    let record = crate::storage::sqlite3_ondisk::read_record(payload)?;
                    if predicate.is_none() {
                        let rowid = match record.values.last() {
                            Some(OwnedValue::Integer(rowid)) => *rowid as u64,
                            _ => unreachable!("index cells should have an integer rowid"),
                        };
                        return Ok(CursorResult::Ok((Some(rowid), Some(record))));
                    }
                    let (key, op) = predicate.as_ref().unwrap();
                    let SeekKey::IndexKey(index_key) = key else {
                        unreachable!("index seek key should be a record");
                    };
                    let found = match op {
                        SeekOp::GT => &record > *index_key,
                        SeekOp::GE => &record >= *index_key,
                        SeekOp::EQ => &record == *index_key,
                    };
                    if found {
                        let rowid = match record.values.last() {
                            Some(OwnedValue::Integer(rowid)) => *rowid as u64,
                            _ => unreachable!("index cells should have an integer rowid"),
                        };
                        return Ok(CursorResult::Ok((Some(rowid), Some(record))));
                    } else {
                        continue;
                    }
                }
            }
        }
    }

    fn seek(
        &mut self,
        key: SeekKey<'_>,
        op: SeekOp,
    ) -> Result<CursorResult<(Option<u64>, Option<OwnedRecord>)>> {
        match self.move_to(key.clone(), op.clone())? {
            CursorResult::Ok(_) => {}
            CursorResult::IO => return Ok(CursorResult::IO),
        };

        {
            let page_rc = self.top_from_stack();
            let page = page_rc.borrow();
            if page.is_locked() {
                return Ok(CursorResult::IO);
            }

            let contents = page.contents.read().unwrap();
            let contents = contents.as_ref().unwrap();

            for cell_idx in 0..contents.cell_count() {
                let cell = contents.cell_get(
                    cell_idx,
                    self.pager.clone(),
                    self.max_local(contents.page_type()),
                    self.min_local(contents.page_type()),
                    self.usable_space(),
                )?;
                match &cell {
                    BTreeCell::TableLeafCell(TableLeafCell {
                        _rowid: cell_rowid,
                        _payload: payload,
                        first_overflow_page: _,
                    }) => {
                        let SeekKey::TableRowId(rowid_key) = key else {
                            unreachable!("table seek key should be a rowid");
                        };
                        self.advance();
                        let found = match op {
                            SeekOp::GT => *cell_rowid > rowid_key,
                            SeekOp::GE => *cell_rowid >= rowid_key,
                            SeekOp::EQ => *cell_rowid == rowid_key,
                        };
                        if found {
                            let record = crate::storage::sqlite3_ondisk::read_record(payload)?;
                            return Ok(CursorResult::Ok((Some(*cell_rowid), Some(record))));
                        }
                    }
                    BTreeCell::IndexLeafCell(IndexLeafCell { payload, .. }) => {
                        let SeekKey::IndexKey(index_key) = key else {
                            unreachable!("index seek key should be a record");
                        };
                        self.advance();
                        let record = crate::storage::sqlite3_ondisk::read_record(payload)?;
                        let found = match op {
                            SeekOp::GT => record > *index_key,
                            SeekOp::GE => record >= *index_key,
                            SeekOp::EQ => record == *index_key,
                        };
                        if found {
                            let rowid = match record.values.last() {
                                Some(OwnedValue::Integer(rowid)) => *rowid as u64,
                                _ => unreachable!("index cells should have an integer rowid"),
                            };
                            return Ok(CursorResult::Ok((Some(rowid), Some(record))));
                        }
                    }
                    cell_type => {
                        unreachable!("unexpected cell type: {:?}", cell_type);
                    }
                }
            }
        }

        // We have now iterated over all cells in the leaf page and found no match.
        let is_index = matches!(key, SeekKey::IndexKey(_));
        if is_index {
            // Unlike tables, indexes store payloads in interior cells as well. self.move_to() always moves to a leaf page, so there are cases where we need to
            // move back up to the parent interior cell and get the next record from there to perform a correct seek.
            // an example of how this can occur:
            //
            // we do an index seek for key K with cmp = SeekOp::GT, meaning we want to seek to the first key that is greater than K.
            // in self.move_to(), we encounter an interior cell with key K' = K+2, and move the left child page, which is a leaf page.
            // the reason we move to the left child page is that we know that in an index, all keys in the left child page are less than K' i.e. less than K+2,
            // meaning that the left subtree may contain a key greater than K, e.g. K+1. however, it is possible that it doesn't, in which case the correct
            // next key is K+2, which is in the parent interior cell.
            //
            // In the seek() method, once we have landed in the leaf page and find that there is no cell with a key greater than K,
            // if we were to return Ok(CursorResult::Ok((None, None))), self.record would be None, which is incorrect, because we already know
            // that there is a record with a key greater than K (K' = K+2) in the parent interior cell. Hence, we need to move back up the tree
            // and get the next matching record from there.
            return self.get_next_record(Some((key, op)));
        }

        Ok(CursorResult::Ok((None, None)))
    }

    fn move_to_root(&mut self) {
        let mem_page = self.pager.read_page(self.root_page).unwrap();
        self.stack.borrow_mut()[0] = Some(mem_page);
        self.cell_indices.borrow_mut()[0] = 0;
        *self.current_page.borrow_mut() = 0;
    }

    fn push_to_stack(&self, page: Rc<RefCell<Page>>) {
        debug!("push to stack {} {}", self.current_page.borrow(), page.borrow().id);
        *self.current_page.borrow_mut() += 1;
        let current = *self.current_page.borrow();
        self.stack.borrow_mut()[current as usize] = Some(page);
        self.cell_indices.borrow_mut()[current as usize] = 0;
    }

    fn pop_from_stack(&self) {
        let current = *self.current_page.borrow();
        debug!("pop_from_stack(current={})", current);
        self.cell_indices.borrow_mut()[current as usize] = 0;
        self.stack.borrow_mut()[current as usize] = None;
        *self.current_page.borrow_mut() -= 1;
    }

    fn top_from_stack(&self) -> Rc<RefCell<Page>> {
        let current = *self.current_page.borrow();
        debug!("top_from_stack(current={})", current);
        self.stack.borrow()[current as usize]
            .as_ref()
            .unwrap()
            .clone()
    }

    fn parent(&self) -> Rc<RefCell<Page>> {
        let current = *self.current_page.borrow();
        self.stack.borrow()[current as usize - 1]
            .as_ref()
            .unwrap()
            .clone()
    }

    fn current(&self) -> usize {
        *self.current_page.borrow() as usize
    }

    fn current_index(&self) -> usize {
        let current = self.current();
        self.cell_indices.borrow()[current]
    }

    fn advance(&self) {
        let current = self.current();
        self.cell_indices.borrow_mut()[current] += 1;
    }

    fn has_parent(&self) -> bool {
        *self.current_page.borrow() > 0
    }

    fn move_to_rightmost(&mut self) -> Result<CursorResult<()>> {
        self.move_to_root();

        loop {
            let mem_page = self.top_from_stack();
            let page_idx = mem_page.borrow().id;
            let page = self.pager.read_page(page_idx)?;
            let page = RefCell::borrow(&page);
            if page.is_locked() {
                return Ok(CursorResult::IO);
            }
            let page = page.contents.read().unwrap();
            let page = page.as_ref().unwrap();
            if page.is_leaf() {
                if page.cell_count() > 0 {
                    self.cell_indices.borrow_mut()[*self.current_page.borrow() as usize] =
                        page.cell_count() - 1;
                }
                return Ok(CursorResult::Ok(()));
            }

            match page.rightmost_pointer() {
                Some(right_most_pointer) => {
                    self.cell_indices.borrow_mut()[*self.current_page.borrow() as usize] =
                        page.cell_count();
                    let mem_page = self.pager.read_page(right_most_pointer as usize).unwrap();
                    self.push_to_stack(mem_page);
                    continue;
                }

                None => {
                    unreachable!("interior page should have a rightmost pointer");
                }
            }
        }
    }

    pub fn move_to(&mut self, key: SeekKey<'_>, cmp: SeekOp) -> Result<CursorResult<()>> {
        // For a table with N rows, we can find any row by row id in O(log(N)) time by starting at the root page and following the B-tree pointers.
        // B-trees consist of interior pages and leaf pages. Interior pages contain pointers to other pages, while leaf pages contain the actual row data.
        //
        // Conceptually, each Interior Cell in a interior page has a rowid and a left child node, and the page itself has a right-most child node.
        // Example: consider an interior page that contains cells C1(rowid=10), C2(rowid=20), C3(rowid=30).
        // - All rows with rowids <= 10 are in the left child node of C1.
        // - All rows with rowids > 10 and <= 20 are in the left child node of C2.
        // - All rows with rowids > 20 and <= 30 are in the left child node of C3.
        // - All rows with rowids > 30 are in the right-most child node of the page.
        //
        // There will generally be multiple levels of interior pages before we reach a leaf page,
        // so we need to follow the interior page pointers until we reach the leaf page that contains the row we are looking for (if it exists).
        //
        // Here's a high-level overview of the algorithm:
        // 1. Since we start at the root page, its cells are all interior cells.
        // 2. We scan the interior cells until we find a cell whose rowid is greater than or equal to the rowid we are looking for.
        // 3. Follow the left child pointer of the cell we found in step 2.
        //    a. In case none of the cells in the page have a rowid greater than or equal to the rowid we are looking for,
        //       we follow the right-most child pointer of the page instead (since all rows with rowids greater than the rowid we are looking for are in the right-most child node).
        // 4. We are now at a new page. If it's another interior page, we repeat the process from step 2. If it's a leaf page, we continue to step 5.
        // 5. We scan the leaf cells in the leaf page until we find the cell whose rowid is equal to the rowid we are looking for.
        //    This cell contains the actual data we are looking for.
        // 6. If we find the cell, we return the record. Otherwise, we return an empty result.
        self.move_to_root();

        loop {
            let page_rc = self.top_from_stack();
            let page = RefCell::borrow(&page_rc);
            if page.is_locked() {
                return Ok(CursorResult::IO);
            }

            let contents = page.contents.read().unwrap();
            let contents = contents.as_ref().unwrap();
            if contents.is_leaf() {
                return Ok(CursorResult::Ok(()));
            }

            let mut found_cell = false;
            for cell_idx in 0..contents.cell_count() {
                match &contents.cell_get(
                    cell_idx,
                    self.pager.clone(),
                    self.max_local(contents.page_type()),
                    self.min_local(contents.page_type()),
                    self.usable_space(),
                )? {
                    BTreeCell::TableInteriorCell(TableInteriorCell {
                        _left_child_page,
                        _rowid,
                    }) => {
                        let SeekKey::TableRowId(rowid_key) = key else {
                            unreachable!("table seek key should be a rowid");
                        };
                        self.advance();
                        let target_leaf_page_is_in_left_subtree = match cmp {
                            SeekOp::GT => rowid_key < *_rowid,
                            SeekOp::GE => rowid_key <= *_rowid,
                            SeekOp::EQ => rowid_key <= *_rowid,
                        };
                        if target_leaf_page_is_in_left_subtree {
                            let mem_page = self.pager.read_page(*_left_child_page as usize)?;
                            self.push_to_stack(mem_page);

                            found_cell = true;
                            break;
                        }
                    }
                    BTreeCell::TableLeafCell(TableLeafCell {
                        _rowid: _,
                        _payload: _,
                        first_overflow_page: _,
                    }) => {
                        unreachable!(
                            "we don't iterate leaf cells while trying to move to a leaf cell"
                        );
                    }
                    BTreeCell::IndexInteriorCell(IndexInteriorCell {
                        left_child_page,
                        payload,
                        ..
                    }) => {
                        let SeekKey::IndexKey(index_key) = key else {
                            unreachable!("index seek key should be a record");
                        };
                        let record = crate::storage::sqlite3_ondisk::read_record(payload)?;
                        let target_leaf_page_is_in_the_left_subtree = match cmp {
                            SeekOp::GT => index_key < &record,
                            SeekOp::GE => index_key <= &record,
                            SeekOp::EQ => index_key <= &record,
                        };
                        if target_leaf_page_is_in_the_left_subtree {
                            let mem_page = self.pager.read_page(*left_child_page as usize).unwrap();
                            self.push_to_stack(mem_page);
                            found_cell = true;
                            break;
                        } else {
                            self.advance();
                        }
                    }
                    BTreeCell::IndexLeafCell(_) => {
                        unreachable!(
                            "we don't iterate leaf cells while trying to move to a leaf cell"
                        );
                    }
                }
            }

            if !found_cell {
                match contents.rightmost_pointer() {
                    Some(right_most_pointer) => {
                        let mem_page = self.pager.read_page(right_most_pointer as usize).unwrap();
                        self.push_to_stack(mem_page);
                        continue;
                    }
                    None => {
                        unreachable!("we shall not go back up! The only way is down the slope");
                    }
                }
            }
        }
    }

    fn insert_into_page(
        &mut self,
        key: &OwnedValue,
        record: &OwnedRecord,
    ) -> Result<CursorResult<()>> {
        loop {
            let state = &self.write_info.state;
            match state {
                WriteState::Start => {
                    let page_ref = self.top_from_stack();
                    let int_key = match key {
                        OwnedValue::Integer(i) => *i as u64,
                        _ => unreachable!("btree tables are indexed by integers!"),
                    };

                    // get page and find cell
                    let (cell_idx, page_type) = {
                        let page = RefCell::borrow(&page_ref);
                        if page.is_locked() {
                            return Ok(CursorResult::IO);
                        }

                        page.set_dirty();
                        self.pager.add_dirty(page.id);

                        let mut page = page.contents.write().unwrap();
                        let page = page.as_mut().unwrap();
                        assert!(matches!(page.page_type(), PageType::TableLeaf));

                        // find cell
                        (self.find_cell(page, int_key), page.page_type())
                    };

                    // TODO: if overwrite drop cell

                    // insert cell

                    let mut cell_payload: Vec<u8> = Vec::new();
                    self.fill_cell_payload(page_type, Some(int_key), &mut cell_payload, record);

                    // insert
                    let overflow = {
                        let page = RefCell::borrow(&page_ref);

                        let mut page = page.contents.write().unwrap();
                        let page = page.as_mut().unwrap();
                        self.insert_into_cell(page, cell_payload.as_slice(), cell_idx);
                        page.overflow_cells.len()
                    };
                    if overflow > 0 {
                        self.write_info.state = WriteState::BalanceStart;
                    } else {
                        self.write_info.state = WriteState::Finish;
                    }
                }
                WriteState::BalanceStart
                | WriteState::BalanceMoveUp
                | WriteState::BalanceGetParentPage => {
                    let res = self.balance_leaf()?;
                    if matches!(res, CursorResult::IO) {
                        return Ok(res);
                    }
                }
                WriteState::Finish => {
                    self.write_info.state = WriteState::Start;
                    return Ok(CursorResult::Ok(()));
                }
            };
        }
    }

    /* insert to postion and shift other pointers */
    fn insert_into_cell(&self, page: &mut PageContent, payload: &[u8], cell_idx: usize) {
        let free = self.compute_free_space(page, RefCell::borrow(&self.database_header));
        let enough_space = payload.len() + 2 <= free as usize;
        if !enough_space {
            // add to overflow cell
            page.overflow_cells.push(OverflowCell {
                index: cell_idx,
                payload: Pin::new(Vec::from(payload)),
            });
            return;
        }

        // TODO: insert into cell payload in internal page
        let pc = self.allocate_cell_space(page, payload.len() as u16);
        let buf = page.as_ptr();

        // copy data
        buf[pc as usize..pc as usize + payload.len()].copy_from_slice(payload);
        //  memmove(pIns+2, pIns, 2*(pPage->nCell - i));
        let (pointer_area_pc_by_idx, _) = page.cell_get_raw_pointer_region();
        let pointer_area_pc_by_idx = pointer_area_pc_by_idx + (2 * cell_idx);

        // move previous pointers forward and insert new pointer there
        let n_cells_forward = 2 * (page.cell_count() - cell_idx);
        if n_cells_forward > 0 {
            buf.copy_within(
                pointer_area_pc_by_idx..pointer_area_pc_by_idx + n_cells_forward,
                pointer_area_pc_by_idx + 2,
            );
        }
        page.write_u16(pointer_area_pc_by_idx, pc);

        // update first byte of content area
        page.write_u16(BTREE_HEADER_OFFSET_CELL_CONTENT, pc);

        // update cell count
        let new_n_cells = (page.cell_count() + 1) as u16;
        page.write_u16(BTREE_HEADER_OFFSET_CELL_COUNT, new_n_cells);
    }

    fn free_cell_range(&self, page: &mut PageContent, offset: u16, len: u16) {
        if page.first_freeblock() == 0 {
            // insert into empty list
            page.write_u16(offset as usize, 0);
            page.write_u16(offset as usize + 2, len);
            page.write_u16(BTREE_HEADER_OFFSET_FREEBLOCK, offset);
            return;
        }
        let first_block = page.first_freeblock();

        if offset < first_block {
            // insert into head of list
            page.write_u16(offset as usize, first_block);
            page.write_u16(offset as usize + 2, len);
            page.write_u16(BTREE_HEADER_OFFSET_FREEBLOCK, offset);
            return;
        }

        if offset <= page.cell_content_area() {
            // extend boundary of content area
            page.write_u16(BTREE_HEADER_OFFSET_FREEBLOCK, page.first_freeblock());
            page.write_u16(BTREE_HEADER_OFFSET_CELL_CONTENT, offset + len);
            return;
        }

        let maxpc = {
            let db_header = self.database_header.borrow();
            let usable_space = (db_header.page_size - db_header.unused_space as u16) as usize;
            usable_space as u16
        };

        let mut pc = first_block;
        let mut prev = first_block;

        while pc <= maxpc && pc < offset {
            let next = page.read_u16(pc as usize);
            prev = pc;
            pc = next;
        }

        if pc >= maxpc {
            // insert into tail
            let offset = offset as usize;
            let prev = prev as usize;
            page.write_u16(prev, offset as u16);
            page.write_u16(offset, 0);
            page.write_u16(offset + 2, len);
        } else {
            // insert in between
            let next = page.read_u16(pc as usize);
            let offset = offset as usize;
            let prev = prev as usize;
            page.write_u16(prev, offset as u16);
            page.write_u16(offset, next);
            page.write_u16(offset + 2, len);
        }
    }

    fn drop_cell(&self, page: &mut PageContent, cell_idx: usize) {
        let (cell_start, cell_len) = page.cell_get_raw_region(
            cell_idx,
            self.max_local(page.page_type()),
            self.min_local(page.page_type()),
            self.usable_space(),
        );
        self.free_cell_range(page, cell_start as u16, cell_len as u16);
        page.write_u16(BTREE_HEADER_OFFSET_CELL_COUNT, page.cell_count() as u16 - 1);
    }

    /// This is a naive algorithm that doesn't try to distribute cells evenly by content.
    /// It will try to split the page in half by keys not by content.
    /// Sqlite tries to have a page at least 40% full.
    fn balance_leaf(&mut self) -> Result<CursorResult<()>> {
        let state = &self.write_info.state;
        match state {
            WriteState::BalanceStart => {
                // drop divider cells and find right pointer
                // NOTE: since we are doing a simple split we only finding the pointer we want to update (right pointer).
                // Right pointer means cell that points to the last page, as we don't really want to drop this one. This one
                // can be a "rightmost pointer" or a "cell".
                // TODO(pere): simplify locking...
                // we always asumme there is a parent
                let current_page = self.top_from_stack();
                let page_rc = RefCell::borrow(&current_page);
                {
                    // check if we don't need to balance

                    {
                        // don't continue if there are no overflow cells
                        let mut page = page_rc.contents.write().unwrap();
                        let page = page.as_mut().unwrap();
                        if page.overflow_cells.is_empty() {
                            self.write_info.state = WriteState::Finish;
                            return Ok(CursorResult::Ok(()));
                        }
                    }
                }

                if !self.has_parent() {
                    drop(page_rc);
                    drop(current_page);
                    self.balance_root();
                    return Ok(CursorResult::Ok(()));
                }
                debug!("Balancing leaf. leaf={}", page_rc.id);

                // Copy of page used to reference cell bytes.
                let page_copy = {
                    let mut page = page_rc.contents.write().unwrap();
                    let page = page.as_mut().unwrap();
                    page.clone()
                };

                // In memory in order copy of all cells in pages we want to balance. For now let's do a 2 page split.
                // Right pointer in interior cells should be converted to regular cells if more than 2 pages are used for balancing.
                let mut scratch_cells = self.write_info.scratch_cells.borrow_mut();
                scratch_cells.clear();

                for cell_idx in 0..page_copy.cell_count() {
                    let (start, len) = page_copy.cell_get_raw_region(
                        cell_idx,
                        self.max_local(page_copy.page_type()),
                        self.min_local(page_copy.page_type()),
                        self.usable_space(),
                    );
                    let buf = page_copy.as_ptr();
                    scratch_cells.push(to_static_buf(&buf[start..start + len]));
                }
                for overflow_cell in &page_copy.overflow_cells {
                    scratch_cells
                        .insert(overflow_cell.index, to_static_buf(&overflow_cell.payload));
                }
                *self.write_info.rightmost_pointer.borrow_mut() =
                    page_copy.rightmost_pointer().clone();

                self.write_info.page_copy.replace(Some(page_copy));

                // allocate new pages and move cells to those new pages
                // split procedure
                let mut page = page_rc.contents.write().unwrap();
                let page = page.as_mut().unwrap();
                assert!(
                    matches!(
                        page.page_type(),
                        PageType::TableLeaf | PageType::TableInterior
                    ),
                    "indexes still not supported "
                );

                let right_page_ref = self.allocate_page(page.page_type());
                let right_page = RefCell::borrow_mut(&right_page_ref);
                let right_page_id = right_page.id;

                self.write_info.new_pages.borrow_mut().clear();
                self.write_info
                    .new_pages
                    .borrow_mut()
                    .push(current_page.clone());
                self.write_info
                    .new_pages
                    .borrow_mut()
                    .push(right_page_ref.clone());

                debug!(
                    "splitting left={} right={}",
                    *self.current_page.borrow(),
                    right_page_id
                );

                self.write_info.state = WriteState::BalanceGetParentPage;
                return Ok(CursorResult::Ok(()));
            }
            WriteState::BalanceGetParentPage => {

                let parent_rc = self.parent();
                if !&parent_rc.borrow().is_locked() {
                    parent_rc.borrow_mut().set_dirty();
                    self.write_info.state = WriteState::BalanceMoveUp;
                    Ok(CursorResult::Ok(()))
                } else {
                    // TODO(pere): maybe request load, it might be that parent was already
                    // requested
                    Ok(CursorResult::IO)
                }
            }
            WriteState::BalanceMoveUp => {
                let parent_ref = self.parent();
                let parent = RefCell::borrow_mut(&parent_ref);

                let (page_type, current_idx) = {
                    let current_page = self.top_from_stack();
                    let page_ref = current_page.borrow();
                    let page = page_ref.contents.read().unwrap();
                    (page.as_ref().unwrap().page_type().clone(), page_ref.id)
                };

                parent.set_dirty();
                self.pager.add_dirty(parent.id);
                let mut parent_contents_lock = parent.contents.write().unwrap();
                let parent_contents = parent_contents_lock.as_mut().unwrap();
                // if this isn't empty next loop won't work
                assert_eq!(parent_contents.overflow_cells.len(), 0);

                // Right page pointer is u32 in right most pointer, and in cell is u32 too, so we can use a *u32 to hold where we want to change this value
                let mut right_pointer = BTREE_HEADER_OFFSET_RIGHTMOST;
                for cell_idx in 0..parent_contents.cell_count() {
                    let cell = parent_contents
                        .cell_get(
                            cell_idx,
                            self.pager.clone(),
                            self.max_local(page_type.clone()),
                            self.min_local(page_type.clone()),
                            self.usable_space(),
                        )
                        .unwrap();
                    let found = match cell {
                        BTreeCell::TableInteriorCell(interior) => {
                            interior._left_child_page as usize == current_idx
                        }
                        _ => unreachable!("Parent should always be a "),
                    };
                    if found {
                        let (start, _len) = parent_contents.cell_get_raw_region(
                            cell_idx,
                            self.max_local(page_type.clone()),
                            self.min_local(page_type.clone()),
                            self.usable_space(),
                        );
                        right_pointer = start;
                        break;
                    }
                }

                let mut new_pages = self.write_info.new_pages.borrow_mut();
                let scratch_cells = self.write_info.scratch_cells.borrow();

                // reset pages
                for page in new_pages.iter() {
                    let page = page.borrow_mut();
                    let mut page = page.contents.write().unwrap();
                    let page = page.as_mut().unwrap();

                    page.write_u16(BTREE_HEADER_OFFSET_FREEBLOCK, 0);
                    page.write_u16(BTREE_HEADER_OFFSET_CELL_COUNT, 0);

                    let db_header = RefCell::borrow(&self.database_header);
                    let cell_content_area_start =
                        db_header.page_size - db_header.unused_space as u16;
                    page.write_u16(BTREE_HEADER_OFFSET_CELL_CONTENT, cell_content_area_start);

                    page.write_u8(BTREE_HEADER_OFFSET_FRAGMENTED, 0);
                    page.write_u32(BTREE_HEADER_OFFSET_RIGHTMOST, 0);
                }

                // distribute cells
                let new_pages_len = new_pages.len();
                let cells_per_page = scratch_cells.len() / new_pages.len();
                let mut current_cell_index = 0_usize;
                let mut divider_cells_index = Vec::new(); /* index to scratch cells that will be used as dividers in order */

                for (i, page) in new_pages.iter_mut().enumerate() {
                    let page = page.borrow_mut();
                    let mut page = page.contents.write().unwrap();
                    let page = page.as_mut().unwrap();

                    let last_page = i == new_pages_len - 1;
                    let cells_to_copy = if last_page {
                        // last cells is remaining pages if division was odd
                        scratch_cells.len() - current_cell_index
                    } else {
                        cells_per_page
                    };

                    let cell_index_range = current_cell_index..current_cell_index + cells_to_copy;
                    for (j, cell_idx) in cell_index_range.enumerate() {
                        let cell = scratch_cells[cell_idx];
                        self.insert_into_cell(page, cell, j);
                    }
                    divider_cells_index.push(current_cell_index + cells_to_copy - 1);
                    current_cell_index += cells_to_copy;
                }
                let is_leaf = {
                    let page = self.top_from_stack();
                    let page = page.borrow();
                    let page = page.contents.read().unwrap();
                    page.as_ref().unwrap().is_leaf()
                };

                // update rightmost pointer for each page if we are in interior page
                if !is_leaf {
                    for page in new_pages.iter_mut().take(new_pages_len - 1) {
                        let page = page.borrow_mut();
                        let mut page = page.contents.write().unwrap();
                        let page = page.as_mut().unwrap();

                        assert!(page.cell_count() == 1);
                        let last_cell = page
                            .cell_get(
                                page.cell_count() - 1,
                                self.pager.clone(),
                                self.max_local(page.page_type()),
                                self.min_local(page.page_type()),
                                self.usable_space(),
                            )
                            .unwrap();
                        let last_cell_pointer = match last_cell {
                            BTreeCell::TableInteriorCell(interior) => interior._left_child_page,
                            _ => unreachable!(),
                        };
                        self.drop_cell(page, page.cell_count() - 1);
                        page.write_u32(BTREE_HEADER_OFFSET_RIGHTMOST, last_cell_pointer);
                    }
                    // last page right most pointer points to previous right most pointer before splitting
                    let last_page = new_pages.last().unwrap();
                    let last_page = RefCell::borrow(&last_page);
                    let mut last_page = last_page.contents.write().unwrap();
                    let last_page = last_page.as_mut().unwrap();
                    last_page.write_u32(
                        BTREE_HEADER_OFFSET_RIGHTMOST,
                        self.write_info.rightmost_pointer.borrow().unwrap(),
                    );
                }

                // insert dividers in parent
                // we can consider dividers the first cell of each page starting from the second page
                for (page_id_index, page) in
                    new_pages.iter_mut().take(new_pages_len - 1).enumerate()
                {
                    let page = page.borrow_mut();
                    let mut contents = page.contents.write().unwrap();
                    let contents = contents.as_mut().unwrap();
                    assert!(contents.cell_count() > 1);
                    let divider_cell_index = divider_cells_index[page_id_index];
                    let cell_payload = scratch_cells[divider_cell_index];
                    let cell = read_btree_cell(
                        cell_payload,
                        &contents.page_type(),
                        0,
                        self.pager.clone(),
                        self.max_local(contents.page_type()),
                        self.min_local(contents.page_type()),
                        self.usable_space(),
                    )
                    .unwrap();

                    if is_leaf {
                        // create a new divider cell and push
                        let key = match cell {
                            BTreeCell::TableLeafCell(leaf) => leaf._rowid,
                            _ => unreachable!(),
                        };
                        let mut divider_cell = Vec::new();
                        divider_cell.extend_from_slice(&(page.id as u32).to_be_bytes());
                        divider_cell.extend(std::iter::repeat(0).take(9));
                        let n = write_varint(&mut divider_cell.as_mut_slice()[4..], key);
                        divider_cell.truncate(4 + n);
                        let parent_cell_idx = self.find_cell(parent_contents, key);
                        self.insert_into_cell(
                            parent_contents,
                            divider_cell.as_slice(),
                            parent_cell_idx,
                        );
                    } else {
                        // move cell
                        let key = match cell {
                            BTreeCell::TableInteriorCell(interior) => interior._rowid,
                            _ => unreachable!(),
                        };
                        let parent_cell_idx = self.find_cell(contents, key);
                        self.insert_into_cell(parent_contents, cell_payload, parent_cell_idx);
                        // self.drop_cell(*page, 0);
                    }
                }

                {
                    // copy last page id to right pointer
                    let last_pointer = new_pages.last().unwrap().borrow().id as u32;
                    parent_contents.write_u32(right_pointer, last_pointer);
                }
                self.pop_from_stack();
                self.write_info.state = WriteState::BalanceStart;
                let _ = self.write_info.page_copy.take();
                Ok(CursorResult::Ok(()))
            }

            _ => unreachable!("invalid balance leaf state {:?}", state),
        }
    }

    fn balance_root(&mut self) {
        /* todo: balance deeper, create child and copy contents of root there. Then split root */
        /* if we are in root page then we just need to create a new root and push key there */

        let new_root_page_ref = self.allocate_page(PageType::TableInterior);
        {
            let new_root_page = RefCell::borrow(&new_root_page_ref);
            let new_root_page_id = new_root_page.id;
            let mut new_root_page_contents = new_root_page.contents.write().unwrap();
            let new_root_page_contents = new_root_page_contents.as_mut().unwrap();
            // point new root right child to previous root
            new_root_page_contents
                .write_u32(BTREE_HEADER_OFFSET_RIGHTMOST, new_root_page_id as u32);
            new_root_page_contents.write_u16(BTREE_HEADER_OFFSET_CELL_COUNT, 0);
        }

        /* swap splitted page buffer with new root buffer so we don't have to update page idx */
        {
            let (root_id, child_id, child) = {
                let page_ref = self.top_from_stack();
                let child = page_ref.clone();
                let mut page_rc = page_ref.borrow_mut();
                let mut new_root_page = new_root_page_ref.borrow_mut();

                // Swap the entire Page structs
                std::mem::swap(&mut page_rc.id, &mut new_root_page.id);

                self.pager.add_dirty(new_root_page.id);
                self.pager.add_dirty(page_rc.id);
                (new_root_page.id, page_rc.id, child)
            };

            debug!("Balancing root. root={}, rightmost={}", root_id, child_id);
            let root = new_root_page_ref.clone();

            self.root_page = root_id;
            *self.current_page.borrow_mut() = -1;
            self.push_to_stack(root.clone());
            self.push_to_stack(child.clone());

            self.pager.put_page(root_id, root);
            self.pager.put_page(child_id, child);

        }
    }

    fn allocate_page(&self, page_type: PageType) -> Rc<RefCell<Page>> {
        let page = self.pager.allocate_page().unwrap();

        {
            // setup btree page
            let contents = RefCell::borrow(&page);
            debug!("allocating page {}", contents.id);
            let mut contents = contents.contents.write().unwrap();
            let contents = contents.as_mut().unwrap();
            let id = page_type as u8;
            contents.write_u8(BTREE_HEADER_OFFSET_TYPE, id);
            contents.write_u16(BTREE_HEADER_OFFSET_FREEBLOCK, 0);
            contents.write_u16(BTREE_HEADER_OFFSET_CELL_COUNT, 0);

            let db_header = RefCell::borrow(&self.database_header);
            let cell_content_area_start = db_header.page_size - db_header.unused_space as u16;
            contents.write_u16(BTREE_HEADER_OFFSET_CELL_CONTENT, cell_content_area_start);

            contents.write_u8(BTREE_HEADER_OFFSET_FRAGMENTED, 0);
            contents.write_u32(BTREE_HEADER_OFFSET_RIGHTMOST, 0);
        }

        page
    }

    fn allocate_overflow_page(&self) -> Rc<RefCell<Page>> {
        let page = self.pager.allocate_page().unwrap();

        {
            // setup overflow page
            let contents = RefCell::borrow(&page);
            let mut contents = contents.contents.write().unwrap();
            let contents = contents.as_mut().unwrap();
            let buf = contents.as_ptr();
            buf.fill(0);
        }

        page
    }

    /*
        Allocate space for a cell on a page.
    */
    fn allocate_cell_space(&self, page_ref: &PageContent, amount: u16) -> u16 {
        let amount = amount as usize;

        let (cell_offset, _) = page_ref.cell_get_raw_pointer_region();
        let gap = cell_offset + 2 * page_ref.cell_count();
        let mut top = page_ref.cell_content_area() as usize;

        // there are free blocks and enough space
        if page_ref.first_freeblock() != 0 && gap + 2 <= top {
            // find slot
            let db_header = RefCell::borrow(&self.database_header);
            let pc = find_free_cell(page_ref, db_header, amount);
            if pc != 0 {
                return pc as u16;
            }
            /* fall through, we might need to defragment */
        }

        if gap + 2 + amount > top {
            // defragment
            self.defragment_page(page_ref, RefCell::borrow(&self.database_header));
            let buf = page_ref.as_ptr();
            top = u16::from_be_bytes([buf[5], buf[6]]) as usize;
        }

        let db_header = RefCell::borrow(&self.database_header);
        top -= amount;

        {
            let buf = page_ref.as_ptr();
            buf[5..7].copy_from_slice(&(top as u16).to_be_bytes());
        }

        let usable_space = (db_header.page_size - db_header.unused_space as u16) as usize;
        assert!(top + amount <= usable_space);
        top as u16
    }

    fn defragment_page(&self, page: &PageContent, db_header: Ref<DatabaseHeader>) {
        let cloned_page = page.clone();
        let usable_space = (db_header.page_size - db_header.unused_space as u16) as u64;
        let mut cbrk = usable_space;

        // TODO: implement fast algorithm

        let last_cell = usable_space - 4;
        let first_cell = {
            let (start, end) = cloned_page.cell_get_raw_pointer_region();
            start + end
        };

        if cloned_page.cell_count() > 0 {
            let page_type = page.page_type();
            let read_buf = cloned_page.as_ptr();
            let write_buf = page.as_ptr();

            for i in 0..cloned_page.cell_count() {
                let cell_offset = page.offset + 8;
                let cell_idx = cell_offset + i * 2;

                let pc = u16::from_be_bytes([read_buf[cell_idx], read_buf[cell_idx + 1]]) as u64;
                if pc > last_cell {
                    unimplemented!("corrupted page");
                }

                assert!(pc <= last_cell);

                let size = match page_type {
                    PageType::TableInterior => {
                        let (_, nr_key) = match read_varint(&read_buf[pc as usize ..]) {
                            Ok(v) => v,
                            Err(_) => todo!(
                                "error while parsing varint from cell, probably treat this as corruption?"
                            ),
                        };
                        4 + nr_key as u64
                    }
                    PageType::TableLeaf => {
                        let (payload_size, nr_payload) = match read_varint(&read_buf[pc as usize..]) {
                            Ok(v) => v,
                            Err(_) => todo!(
                                "error while parsing varint from cell, probably treat this as corruption?"
                            ),
                        };
                        let (_, nr_key) = match read_varint(&read_buf[pc as usize + nr_payload..]) {
                            Ok(v) => v,
                            Err(_) => todo!(
                                "error while parsing varint from cell, probably treat this as corruption?"
                            ),
                        };
                        // TODO: add overflow page calculation
                        payload_size + nr_payload as u64 + nr_key as u64
                    }
                    PageType::IndexInterior => todo!(),
                    PageType::IndexLeaf => todo!(),
                };
                cbrk -= size;
                if cbrk < first_cell as u64 || pc + size > usable_space {
                    todo!("corrupt");
                }
                assert!(cbrk + size <= usable_space && cbrk >= first_cell as u64);
                // set new pointer
                write_buf[cell_idx..cell_idx + 2].copy_from_slice(&(cbrk as u16).to_be_bytes());
                // copy payload
                write_buf[cbrk as usize..cbrk as usize + size as usize]
                    .copy_from_slice(&read_buf[pc as usize..pc as usize + size as usize]);
            }
        }

        // assert!( nfree >= 0 );
        // if( data[hdr+7]+cbrk-iCellFirst!=pPage->nFree ){
        //   return SQLITE_CORRUPT_PAGE(pPage);
        // }
        assert!(cbrk >= first_cell as u64);
        let write_buf = page.as_ptr();

        // set new first byte of cell content
        write_buf[5..7].copy_from_slice(&(cbrk as u16).to_be_bytes());
        // set free block to 0, unused spaced can be retrieved from gap between cell pointer end and content start
        write_buf[1] = 0;
        write_buf[2] = 0;
        // set unused space to 0
        let first_cell = cloned_page.cell_content_area() as u64;
        assert!(first_cell <= cbrk);
        write_buf[first_cell as usize..cbrk as usize].fill(0);
    }

    // Free blocks can be zero, meaning the "real free space" that can be used to allocate is expected to be between first cell byte
    // and end of cell pointer area.
    #[allow(unused_assignments)]
    fn compute_free_space(&self, page: &PageContent, db_header: Ref<DatabaseHeader>) -> u16 {
        let buf = page.as_ptr();

        let usable_space = (db_header.page_size - db_header.unused_space as u16) as usize;
        let mut first_byte_in_cell_content = page.cell_content_area();
        if first_byte_in_cell_content == 0 {
            first_byte_in_cell_content = u16::MAX;
        }

        let fragmented_free_bytes = page.num_frag_free_bytes();
        let free_block_pointer = page.first_freeblock();
        let ncell = page.cell_count();

        // 8 + 4 == header end
        let first_cell = (page.offset + 8 + 4 + (2 * ncell)) as u16;

        let mut nfree = fragmented_free_bytes as usize + first_byte_in_cell_content as usize;

        let mut pc = free_block_pointer as usize;
        if pc > 0 {
            if pc < first_byte_in_cell_content as usize {
                // corrupt
                todo!("corrupted page");
            }

            let mut next = 0;
            let mut size = 0;
            loop {
                // TODO: check corruption icellast
                next = u16::from_be_bytes(buf[pc..pc + 2].try_into().unwrap()) as usize;
                size = u16::from_be_bytes(buf[pc + 2..pc + 4].try_into().unwrap()) as usize;
                nfree += size;
                if next <= pc + size + 3 {
                    break;
                }
                pc = next;
            }

            if next > 0 {
                todo!("corrupted page ascending order");
            }

            if pc + size > usable_space {
                todo!("corrupted page last freeblock extends last page end");
            }
        }

        // if( nFree>usableSize || nFree<iCellFirst ){
        //   return SQLITE_CORRUPT_PAGE(pPage);
        // }
        // don't count header and cell pointers?
        nfree -= first_cell as usize;
        nfree as u16
    }

    fn fill_cell_payload(
        &self,
        page_type: PageType,
        int_key: Option<u64>,
        cell_payload: &mut Vec<u8>,
        record: &OwnedRecord,
    ) {
        assert!(matches!(
            page_type,
            PageType::TableLeaf | PageType::IndexLeaf
        ));
        // TODO: make record raw from start, having to serialize is not good
        let mut record_buf = Vec::new();
        record.serialize(&mut record_buf);

        // fill in header
        if matches!(page_type, PageType::TableLeaf) {
            let int_key = int_key.unwrap();
            write_varint_to_vec(record_buf.len() as u64, cell_payload);
            write_varint_to_vec(int_key, cell_payload);
        } else {
            write_varint_to_vec(record_buf.len() as u64, cell_payload);
        }

        let max_local = self.max_local(page_type.clone());
        if record_buf.len() <= max_local {
            // enough allowed space to fit inside a btree page
            cell_payload.extend_from_slice(record_buf.as_slice());
            cell_payload.resize(cell_payload.len() + 4, 0);
            return;
        }

        let min_local = self.min_local(page_type);
        let mut space_left = min_local + (record_buf.len() - min_local) % (self.usable_space() - 4);

        if space_left > max_local {
            space_left = min_local;
        }

        // cell_size must be equal to first value of space_left as this will be the bytes copied to non-overflow page.
        let cell_size = space_left + cell_payload.len() + 4; // 4 is the number of bytes of pointer to first overflow page
        let mut to_copy_buffer = record_buf.as_slice();

        let prev_size = cell_payload.len();
        cell_payload.resize(prev_size + space_left + 4, 0);
        let mut pointer = unsafe { cell_payload.as_mut_ptr().add(prev_size) };
        let mut pointer_to_next = unsafe { cell_payload.as_mut_ptr().add(prev_size + space_left) };
        let mut overflow_pages = Vec::new();

        loop {
            let to_copy = space_left.min(to_copy_buffer.len());
            unsafe { std::ptr::copy(to_copy_buffer.as_ptr(), pointer, to_copy) };

            let left = to_copy_buffer.len() - to_copy;
            if left == 0 {
                break;
            }

            // we still have bytes to add, we will need to allocate new overflow page
            let overflow_page = self.allocate_overflow_page();
            overflow_pages.push(overflow_page.clone());
            {
                let page = overflow_page.borrow();
                let mut contents_lock = page.contents.write().unwrap();
                let contents = contents_lock.as_mut().unwrap();

                let buf = contents.as_ptr();
                let id = page.id as u32;
                let as_bytes = id.to_be_bytes();
                // update pointer to new overflow page
                unsafe { std::ptr::copy(as_bytes.as_ptr(), pointer_to_next, 4) };

                pointer = unsafe { buf.as_mut_ptr().add(4) };
                pointer_to_next = buf.as_mut_ptr();
                space_left = self.usable_space() - 4;
            }

            to_copy_buffer = &to_copy_buffer[to_copy..];
        }

        assert_eq!(cell_size, cell_payload.len());
    }

    fn max_local(&self, page_type: PageType) -> usize {
        let usable_space = self.usable_space();
        match page_type {
            PageType::IndexInterior | PageType::TableInterior => {
                (usable_space - 12) * 64 / 255 - 23
            }
            PageType::IndexLeaf | PageType::TableLeaf => usable_space - 35,
        }
    }

    fn min_local(&self, page_type: PageType) -> usize {
        let usable_space = self.usable_space();
        match page_type {
            PageType::IndexInterior | PageType::TableInterior => {
                (usable_space - 12) * 32 / 255 - 23
            }
            PageType::IndexLeaf | PageType::TableLeaf => (usable_space - 12) * 32 / 255 - 23,
        }
    }

    fn usable_space(&self) -> usize {
        let db_header = RefCell::borrow(&self.database_header);
        (db_header.page_size - db_header.unused_space as u16) as usize
    }

    fn find_cell(&self, page: &PageContent, int_key: u64) -> usize {
        let mut cell_idx = 0;
        let cell_count = page.cell_count();
        while cell_idx < cell_count {
            match page
                .cell_get(
                    cell_idx,
                    self.pager.clone(),
                    self.max_local(page.page_type()),
                    self.min_local(page.page_type()),
                    self.usable_space(),
                )
                .unwrap()
            {
                BTreeCell::TableLeafCell(cell) => {
                    if int_key <= cell._rowid {
                        break;
                    }
                }
                BTreeCell::TableInteriorCell(cell) => {
                    if int_key <= cell._rowid {
                        break;
                    }
                }
                _ => todo!(),
            }
            cell_idx += 1;
        }
        cell_idx
    }
}

fn find_free_cell(page_ref: &PageContent, db_header: Ref<DatabaseHeader>, amount: usize) -> usize {
    // NOTE: freelist is in ascending order of keys and pc
    // unuse_space is reserved bytes at the end of page, therefore we must substract from maxpc
    let mut pc = page_ref.first_freeblock() as usize;

    let buf = page_ref.as_ptr();

    let usable_space = (db_header.page_size - db_header.unused_space as u16) as usize;
    let maxpc = usable_space - amount;
    let mut found = false;
    while pc <= maxpc {
        let next = u16::from_be_bytes(buf[pc..pc + 2].try_into().unwrap());
        let size = u16::from_be_bytes(buf[pc + 2..pc + 4].try_into().unwrap());
        if amount <= size as usize {
            found = true;
            break;
        }
        pc = next as usize;
    }
    if !found {
        0
    } else {
        pc
    }
}

impl Cursor for BTreeCursor {
    fn seek_to_last(&mut self) -> Result<CursorResult<()>> {
        self.move_to_rightmost()?;
        match self.get_next_record(None)? {
            CursorResult::Ok((rowid, next)) => {
                if rowid.is_none() {
                    match self.is_empty_table()? {
                        CursorResult::Ok(is_empty) => {
                            assert!(is_empty)
                        }
                        CursorResult::IO => (),
                    }
                }
                self.rowid.replace(rowid);
                self.record.replace(next);
                Ok(CursorResult::Ok(()))
            }
            CursorResult::IO => Ok(CursorResult::IO),
        }
    }

    fn is_empty(&self) -> bool {
        self.record.borrow().is_none()
    }

    fn rewind(&mut self) -> Result<CursorResult<()>> {
        self.move_to_root();

        match self.get_next_record(None)? {
            CursorResult::Ok((rowid, next)) => {
                self.rowid.replace(rowid);
                self.record.replace(next);
                Ok(CursorResult::Ok(()))
            }
            CursorResult::IO => Ok(CursorResult::IO),
        }
    }

    fn next(&mut self) -> Result<CursorResult<()>> {
        match self.get_next_record(None)? {
            CursorResult::Ok((rowid, next)) => {
                self.rowid.replace(rowid);
                self.record.replace(next);
                Ok(CursorResult::Ok(()))
            }
            CursorResult::IO => Ok(CursorResult::IO),
        }
    }

    fn wait_for_completion(&mut self) -> Result<()> {
        // TODO: Wait for pager I/O to complete
        Ok(())
    }

    fn rowid(&self) -> Result<Option<u64>> {
        Ok(*self.rowid.borrow())
    }

    fn seek(&mut self, key: SeekKey<'_>, op: SeekOp) -> Result<CursorResult<bool>> {
        match self.seek(key, op)? {
            CursorResult::Ok((rowid, record)) => {
                self.rowid.replace(rowid);
                self.record.replace(record);
                Ok(CursorResult::Ok(rowid.is_some()))
            }
            CursorResult::IO => Ok(CursorResult::IO),
        }
    }

    fn record(&self) -> Result<Ref<Option<OwnedRecord>>> {
        Ok(self.record.borrow())
    }

    fn insert(
        &mut self,
        key: &OwnedValue,
        _record: &OwnedRecord,
        moved_before: bool, /* Indicate whether it's necessary to traverse to find the leaf page */
    ) -> Result<CursorResult<()>> {
        let int_key = match key {
            OwnedValue::Integer(i) => i,
            _ => unreachable!("btree tables are indexed by integers!"),
        };
        if !moved_before {
            match self.move_to(SeekKey::TableRowId(*int_key as u64), SeekOp::EQ)? {
                CursorResult::Ok(_) => {}
                CursorResult::IO => return Ok(CursorResult::IO),
            };
        }

        match self.insert_into_page(key, _record)? {
            CursorResult::Ok(_) => Ok(CursorResult::Ok(())),
            CursorResult::IO => Ok(CursorResult::IO),
        }
    }

    fn set_null_flag(&mut self, flag: bool) {
        self.null_flag = flag;
    }

    fn get_null_flag(&self) -> bool {
        self.null_flag
    }

    fn exists(&mut self, key: &OwnedValue) -> Result<CursorResult<bool>> {
        let int_key = match key {
            OwnedValue::Integer(i) => i,
            _ => unreachable!("btree tables are indexed by integers!"),
        };
        match self.move_to(SeekKey::TableRowId(*int_key as u64), SeekOp::EQ)? {
            CursorResult::Ok(_) => {}
            CursorResult::IO => return Ok(CursorResult::IO),
        };
        let page_ref = self.top_from_stack();
        let page = RefCell::borrow(&page_ref);
        if page.is_locked() {
            // TODO(pere); request load
            return Ok(CursorResult::IO);
        }

        let page = page.contents.read().unwrap();
        let page = page.as_ref().unwrap();

        // find cell
        let int_key = match key {
            OwnedValue::Integer(i) => *i as u64,
            _ => unreachable!("btree tables are indexed by integers!"),
        };
        let cell_idx = self.find_cell(page, int_key);
        if cell_idx >= page.cell_count() {
            Ok(CursorResult::Ok(false))
        } else {
            let equals = match &page.cell_get(
                cell_idx,
                self.pager.clone(),
                self.max_local(page.page_type()),
                self.min_local(page.page_type()),
                self.usable_space(),
            )? {
                BTreeCell::TableLeafCell(l) => l._rowid == int_key,
                _ => unreachable!(),
            };
            Ok(CursorResult::Ok(equals))
        }
    }
}

fn to_static_buf(buf: &[u8]) -> &'static [u8] {
    unsafe { std::mem::transmute::<&[u8], &'static [u8]>(buf) }
}
