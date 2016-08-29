use Result;
use DocId;
use std::io;
use schema::Schema;
use schema::Document;
use schema::Term;
use core::SegmentInfo;
use core::Segment;
use core::SerializableSegment;
use postings::PostingsWriter;
use fastfield::U32FastFieldsWriter;
use schema::Field;
use schema::FieldEntry;
use schema::FieldValue;
use schema::FieldType;
use schema::TextIndexingOptions;
use postings::SpecializedPostingsWriter;
use postings::{NothingRecorder, TermFrequencyRecorder, TFAndPositionRecorder};
use indexer::segment_serializer::SegmentSerializer;
use postings::BlockStore;

pub struct SegmentWriter<'a> {
	block_store: &'a mut BlockStore,
    max_doc: DocId,
	per_field_postings_writers: Vec<Box<PostingsWriter>>,
	segment_serializer: SegmentSerializer,
	fast_field_writers: U32FastFieldsWriter,
	fieldnorms_writer: U32FastFieldsWriter,
}

fn create_fieldnorms_writer(schema: &Schema) -> U32FastFieldsWriter {
	let u32_fields: Vec<Field> = schema.fields()
		.iter()
		.enumerate()
		.filter(|&(_, field_entry)| field_entry.is_indexed()) 
		.map(|(field_id, _)| Field(field_id as u8))
		.collect();
	U32FastFieldsWriter::new(u32_fields)
}

fn posting_from_field_entry(field_entry: &FieldEntry) -> Box<PostingsWriter> {
	match field_entry.field_type() {
		&FieldType::Str(ref text_options) => {
			match text_options.get_indexing_options() {
				TextIndexingOptions::TokenizedWithFreq => {
					SpecializedPostingsWriter::<TermFrequencyRecorder>::new_boxed()
				}
				TextIndexingOptions::TokenizedWithFreqAndPosition => {
					SpecializedPostingsWriter::<TFAndPositionRecorder>::new_boxed()
				}
				_ => {
					SpecializedPostingsWriter::<NothingRecorder>::new_boxed()
				}
			}
		} 
		&FieldType::U32(_) => {
			SpecializedPostingsWriter::<NothingRecorder>::new_boxed()
		}
	}
}



impl<'a> SegmentWriter<'a> {

	pub fn for_segment(block_store: &'a mut BlockStore, mut segment: Segment, schema: &Schema) -> Result<SegmentWriter<'a>> {
		let segment_serializer = try!(SegmentSerializer::for_segment(&mut segment));
		let per_field_postings_writers = schema.fields()
			  .iter()
			  .map(|field_entry| {
				  posting_from_field_entry(field_entry)
			  })
			  .collect();
		Ok(SegmentWriter {
			block_store: block_store,
			max_doc: 0,
			per_field_postings_writers: per_field_postings_writers,
			fieldnorms_writer: create_fieldnorms_writer(schema),
			segment_serializer: segment_serializer,
			fast_field_writers: U32FastFieldsWriter::from_schema(schema),
		})
	}
	
	// Write on disk all of the stuff that
	// is still on RAM :
	// - the dictionary in an fst
	// - the postings
	// - the segment info
	// The segment writer cannot be used after this, which is
	// enforced by the fact that "self" is moved.
	pub fn finalize(mut self,) -> Result<()> {
		let segment_info = self.segment_info();
		for per_field_postings_writer in self.per_field_postings_writers.iter_mut() {
			per_field_postings_writer.close(&mut self.block_store);
		}
		write(&mut self.block_store,
			  &self.per_field_postings_writers,
			  &self.fast_field_writers,
			  &self.fieldnorms_writer,
			  segment_info,
			  self.segment_serializer)
	}
	
	pub fn is_buffer_full(&self,) -> bool {
		self.block_store.num_free_blocks() < 100_000
	}
	
    pub fn add_document(&mut self, doc: &Document, schema: &Schema) -> io::Result<()> {
        let doc_id = self.max_doc;
        for (field, field_values) in doc.get_sorted_fields() {
			let field_posting_writer: &mut Box<PostingsWriter> = &mut self.per_field_postings_writers[field.0 as usize];
			let field_options = schema.get_field_entry(field);
			match *field_options.field_type() {
				FieldType::Str(ref text_options) => {
					let mut num_tokens = 0u32;
					if text_options.get_indexing_options().is_tokenized() {
						num_tokens = field_posting_writer.index_text(&mut self.block_store, doc_id, field, &field_values);
					}
					else {
						for field_value in field_values {
							let term = Term::from_field_text(field, field_value.value().text());
							field_posting_writer.suscribe(&mut self.block_store, doc_id, 0, &term);
							num_tokens += 1u32;
						}
					}		
					self.fieldnorms_writer
						.get_field_writer(field)
						.map(|field_norms_writer| {
							field_norms_writer.add_val(num_tokens as u32)
						});
				}
				FieldType::U32(ref u32_options) => {
					if u32_options.is_indexed() {
						for field_value in field_values {
							let term = Term::from_field_u32(field_value.field(), field_value.value().u32_value());
							field_posting_writer.suscribe(&mut self.block_store, doc_id, 0, &term);
						}
					}
				}
			}

		}
		
		self.fieldnorms_writer.fill_val_up_to(doc_id);
		
		self.fast_field_writers.add_document(&doc);
		let stored_fieldvalues: Vec<&FieldValue> = doc
			.get_fields()
			.iter()
			.filter(|field_value| schema.get_field_entry(field_value.field()).is_stored())
			.collect();
		let doc_writer = self.segment_serializer.get_store_writer();
		try!(doc_writer.store(&stored_fieldvalues));
        self.max_doc += 1;
		Ok(())
    }


	fn segment_info(&self,) -> SegmentInfo {
		SegmentInfo {
			max_doc: self.max_doc
		}
	}

	pub fn max_doc(&self,) -> u32 {
		self.max_doc
	}

}

fn write(block_store: &BlockStore,
	 	 per_field_postings_writers: &Vec<Box<PostingsWriter>>,
		 fast_field_writers: &U32FastFieldsWriter,
		 fieldnorms_writer: &U32FastFieldsWriter,
		 segment_info: SegmentInfo,
	  	mut serializer: SegmentSerializer) -> Result<()> {
		for per_field_postings_writer in per_field_postings_writers.iter() {
			try!(per_field_postings_writer.serialize(block_store, serializer.get_postings_serializer()));
		}
		try!(fast_field_writers.serialize(serializer.get_fast_field_serializer()));
		try!(fieldnorms_writer.serialize(serializer.get_fieldnorms_serializer()));
		try!(serializer.write_segment_info(&segment_info));
		try!(serializer.close());
		Ok(())
}

impl<'a> SerializableSegment for SegmentWriter<'a> {
	fn write(&self, serializer: SegmentSerializer) -> Result<()> {
		write(&self.block_store,
			  &self.per_field_postings_writers,
		      &self.fast_field_writers,
			  &self.fieldnorms_writer,
			  self.segment_info(),
		      serializer)
	}
}