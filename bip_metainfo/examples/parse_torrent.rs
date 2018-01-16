extern crate bip_metainfo;
#[macro_use]
extern crate bip_bencode;

use std::fs::File;
use std::io::prelude::*;

use bip_metainfo::{Metainfo, MetainfoBuilder, InfoBuilder};
use bip_bencode::{BencodeMut, BMutAccess, BencodeRefKind, BDictAccess, BListAccess, BRefAccess};
use bip_metainfo::error::ParseErrorKind::BencodeConvert;
use bip_bencode::BencodeConvertError;
use bip_metainfo::error::{ParseError, ParseResult};
use std::borrow::Cow;
use std::borrow::Borrow;

fn print_by_kind(v: &BencodeMut) {
    match v.kind() {
        BencodeRefKind::Bytes(bytes) => {
            match std::str::from_utf8(bytes) {
                Ok(val_str) => {
                    print!("{}", val_str);
                }
                Err(_) => {
                    print!("Bytes({})", bytes.len());
                }
            };
        }
        BencodeRefKind::Int(int) => {
            print!("{}", int);
        }
        BencodeRefKind::List(list) => {
            print!("\n");
            for item in list.into_iter() {
                print_by_kind(item);
            }
        }
        BencodeRefKind::Dict(dict) => {
            print!("\n");
            let list = dict.to_list();

            for &(k, v) in list.iter() {
                let key_str = std::str::from_utf8(k).unwrap();
                print!("{}: ", key_str);

                print_by_kind(v);
            }
        }
    };

    print!("\n");
}

fn main() {
    let builder = MetainfoBuilder::new()
        .set_comment(Some("Just Some Comment"));

    let mut root = builder.root;

    {
        let root_access = root.dict_mut().unwrap();
        const COMMENT_KEY: &'static [u8] = b"comment";
        const INFO_KEY: &'static [u8] = b"info";
        const PIECE_LENGTH_KEY: &'static [u8] = b"piece length";
        const PIECES_KEY: &'static [u8] = b"pieces";
        const PRIVATE_KEY: &'static [u8] = b"private";
        const NAME_KEY: &'static [u8] = b"name";
        const LENGTH_KEY: &'static [u8] = b"length";
        const HASH: &'static [u8; 20] = b"00000000000000000000";

        let mut info = BencodeMut::new_dict();

        {
            let info_access = info.dict_mut().unwrap();
            info_access.insert(PIECE_LENGTH_KEY.into(), ben_int!(100));

            info_access.insert(PIECES_KEY.into(), ben_bytes!(&HASH[..]));

            info_access.insert(LENGTH_KEY.into(), ben_int!(100));
            info_access.insert(NAME_KEY.into(), ben_bytes!("foo.txt"));
        }

        root_access.insert(INFO_KEY.into(), info);
    }

    print_by_kind(&root);
    let buffer = root.encode();

    Metainfo::from_bytes(&buffer)
        .and_then(|metainfo| {
            println!("{:?}", metainfo.comment());

            Ok(())
        })
        .or_else(|err| {
            match &err {
                &ParseError(ref err, _) => {
                    println!("{:?}", err);
                }
                _ => ()
            };

            Err(err)
        });
}