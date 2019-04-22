use std::io::{stdin};
use std::path::{Path};
use std::sync::mpsc::{Sender, channel};
use std::collections::hash_map::{HashMap, Entry};
use std::fs::{self, DirEntry};
use std::io::prelude::*;
use clap::{Arg, App};
use rayon::prelude::*;
use ddh::{Fileinfo, PrintFmt, Verbosity, HashMode};

fn main() {
    let arguments = App::new("Directory Difference hTool")
                        .version("0.9.9")
                        .author("Jon Moroney jmoroney@hawaii.edu")
                        .about("Compare and contrast directories.\nExample invocation: ddh /home/jon/downloads /home/jon/documents -p shared")
                        .arg(Arg::with_name("directories")
                               .short("d")
                               .long("directories")
                               .value_name("Directories")
                               .help("Directories to parse")
                               .min_values(1)
                               .required(true)
                               .takes_value(true)
                               .index(1))
                        .arg(Arg::with_name("Blocksize")
                               .short("bs")
                               .long("blocksize")
                               .case_insensitive(true)
                               .takes_value(true)
                               .max_values(1)
                               .possible_values(&["B", "K", "M", "G"])
                               .help("Sets the display blocksize to Bytes, Kilobytes, Megabytes or Gigabytes. Default is Kilobytes."))
                        .arg(Arg::with_name("Verbosity")
                                .short("v")
                                .long("verbosity")
                                .possible_values(&["quiet", "duplicates", "all"])
                                .case_insensitive(true)
                                .takes_value(true)
                                .help("Sets verbosity for printed output."))
                        .arg(Arg::with_name("Output")
                                .short("o")
                                .long("output")
                                .takes_value(true)
                                .max_values(1)
                                .help("Sets file to save all output. Use 'no' for no file output."))
                        .arg(Arg::with_name("Format")
                                .short("f")
                                .long("format")
                                .possible_values(&["standard", "json", "off"])
                                .takes_value(true)
                                .max_values(1)
                                .help("Sets output format."))
                        .get_matches();

    let (sender, receiver) = channel();
    let search_dirs: Vec<_> = arguments.values_of("directories").unwrap()
    .collect();

    //Search over user supplied directories
    search_dirs.par_iter().for_each_with(sender, |s, search_dir| {
        stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
            traverse_and_spawn(Path::new(&search_dir), s.clone());
        });
    });
    
    //Collect Fileinfo entries in a HashMap of vectors. Each vector corrosponds to a specific flie length
    let mut files_of_lengths: HashMap<u64, Vec<Fileinfo>> = HashMap::new();
    for entry in receiver.iter(){
        match files_of_lengths.entry(entry.get_length()) {
            Entry::Vacant(e) => { e.insert(vec![entry]); },
            Entry::Occupied(mut e) => { e.get_mut().push(entry); }
        }
    }

    //Compare them files
    let complete_files: Vec<Fileinfo> = files_of_lengths.into_par_iter().map(|x| //For each vector diff and compare on x.0 (length) and x.1 the vector
        differentiate_and_consolidate(x.0, x.1)
    ).flatten().collect();
    //Get duplicates and singletons
    let (shared_files, unique_files): (Vec<&Fileinfo>, Vec<&Fileinfo>) = complete_files.par_iter().partition(|&x| x.file_paths.len()>1);
    process_full_output(&shared_files, &unique_files, &complete_files, &arguments);
}

fn traverse_and_spawn(current_path: &Path, sender: Sender<Fileinfo>) -> (){
    if !current_path.exists(){
        return
    }
    if current_path.symlink_metadata().expect("Error getting Symlink Metadata").file_type().is_dir(){
        let mut paths: Vec<DirEntry> = Vec::new();
        match fs::read_dir(current_path) {
                Ok(read_dir_results) => read_dir_results
                .filter(|x| x.is_ok())
                .for_each(
                    |x| paths.push(x.unwrap())
                    ),
                Err(e) => println!("Skipping {:?}. {:?}", current_path, e.kind()),
            }
        paths.into_par_iter().for_each_with(sender, |s, dir_entry| {
            stacker::maybe_grow(32 * 1024, 1024 * 1024, || {
                traverse_and_spawn(dir_entry.path().as_path(), s.clone());
            });
        });
    } else if current_path
    .symlink_metadata()
    .expect("Error getting Symlink Metadata")
    .file_type()
    .is_file(){
        sender.send(Fileinfo::new(None, None, current_path.metadata().expect("Error with current path length").len(), current_path.to_path_buf())).expect("Error sending new fileinfo");
    } else {}
}

fn differentiate_and_consolidate(file_length: u64, mut files: Vec<Fileinfo>) -> Vec<Fileinfo>{
    if file_length==0 || files.len()==0{
        return files
    }
    match files.len(){
        1 => return files,
        n if n>1 => {
            //Hash stage one
            files.par_iter_mut().for_each(|file_ref| {
                let hash = file_ref.generate_hash(HashMode::Partial);
                file_ref.set_partial_hash(hash);
            });
            files.par_sort_unstable_by(|a, b| b.get_partial_hash().cmp(&a.get_partial_hash())); //O(nlog(n))
            if file_length>4096 /*4KB*/ { //only hash again if we are not done hashing
                files.dedup_by(|a, b| if a==b{ //O(n)
                    a.set_full_hash(Some(1));
                    b.set_full_hash(Some(1));
                    false
                }else{false});
                files.par_iter_mut().filter(|x| x.get_full_hash().is_some()).for_each(|file_ref| {
                    let hash = file_ref.generate_hash(HashMode::Full);
                    file_ref.set_full_hash(hash);
                });
            }
        },
        _ => {panic!("Somehow a vector of negative length was created. Please report this as a bug");}
    }
    files.dedup_by(|a, b| if a==b{ //O(n)
        b.file_paths.extend(a.file_paths.drain(0..));
        true
    }else{false});
    files
}

fn process_full_output(shared_files: &Vec<&Fileinfo>, unique_files: &Vec<&Fileinfo>, complete_files: &Vec<Fileinfo>, arguments: &clap::ArgMatches) ->(){
    //Get constants
    let blocksize = match arguments.value_of("Blocksize").unwrap_or(""){"B" => "Bytes", "K" => "Kilobytes", "M" => "Megabytes", "G" => "Gigabytes", _ => "Megabytes"};
    let display_power = match blocksize{"Bytes" => 0, "Kilobytes" => 1, "Megabytes" => 2, "Gigabytes" => 3, _ => 2};
    let display_divisor =  1024u64.pow(display_power);
    let fmt = match arguments.value_of("Format").unwrap_or(""){
        "standard" => PrintFmt::Standard,
        "json" => PrintFmt::Json,
        _ => PrintFmt::Standard};
    let verbosity = match arguments.value_of("Verbosity").unwrap_or(""){
        "quiet" => Verbosity::Quiet,
        "duplicates" => Verbosity::Duplicates,
        "all" => Verbosity::All,
        _ => Verbosity::Quiet};

    //Print primary output.
    println!("{} Total files (with duplicates): {} {}", complete_files.par_iter()
    .map(|x| x.file_paths.len() as u64)
    .sum::<u64>(),
    complete_files.par_iter()
    .map(|x| (x.file_paths.len() as u64)*x.get_length())
    .sum::<u64>()/(display_divisor),
    blocksize);
    println!("{} Total files (without duplicates): {} {}", complete_files.len(), complete_files.par_iter()
    .map(|x| x.get_length())
    .sum::<u64>()/(display_divisor),
    blocksize);
    println!("{} Single instance files: {} {}",unique_files.len(), unique_files.par_iter()
    .map(|x| x.get_length())
    .sum::<u64>()/(display_divisor),
    blocksize);
    println!("{} Shared instance files: {} {} ({} instances)", shared_files.len(), shared_files.par_iter()
    .map(|x| x.get_length())
    .sum::<u64>()/(display_divisor),
    blocksize, shared_files.par_iter()
    .map(|x| x.file_paths.len() as u64)
    .sum::<u64>());

    //Print extended output if desired
    match (fmt, verbosity) {
        (_, Verbosity::Quiet) => {},
        (PrintFmt::Standard, Verbosity::Duplicates) => {
            println!("Shared instance files and instance locations"); shared_files.iter().for_each(|x| {
            println!("instances of {} with file length {}:", x.get_file_name(), x.get_length());
            x.file_paths.par_iter().for_each(|y| println!("\t{}", y.canonicalize().unwrap().to_str().unwrap()));})
        },
        (PrintFmt::Standard, Verbosity::All) => {
            println!("Single instance files"); unique_files.par_iter()
            .for_each(|x| println!("{}", x.file_paths.iter().next().unwrap().canonicalize().unwrap().to_str().unwrap()));
            println!("Shared instance files and instance locations"); shared_files.iter().for_each(|x| {
            println!("instances of {} with file length {}:", x.get_file_name(), x.get_length());
            x.file_paths.par_iter().for_each(|y| println!("\t{}", y.canonicalize().unwrap().to_str().unwrap()));})
        },
        (PrintFmt::Json, Verbosity::Duplicates) => {
            println!("{}", serde_json::to_string(shared_files).unwrap_or("".to_string()));
        },
        (PrintFmt::Json, Verbosity::All) => {
            println!("{}", serde_json::to_string(complete_files).unwrap_or("".to_string()));
        },
        _ => {},
    }

    //Check if output file is defined. If it exists ask for overwrite.
    match arguments.value_of("Output").unwrap_or("Results.txt"){
        "no" => {},
        destination_string => {
            match fs::File::open(destination_string) {
                    Ok(_f) => { //File exists.
                    println!("---");
                    println!("File {} already exists.", destination_string);
                    println!("Overwrite? Y/N");
                    let mut input = String::new();
                    match stdin().read_line(&mut input) {
                        Ok(_n) => {
                            match input.chars().next().unwrap_or(' ') {
                                'n' | 'N' => {println!("Exiting."); return;}
                                'y' | 'Y' => {println!("Over writing {}", destination_string);}
                                _ => {println!("Exiting."); return;}
                            }
                        }
                        Err(_e) => {println!("Error encountered reading user input. Err: {}", _e);},
                    }
                },
                Err(_e) => {
                    match fs::File::create(destination_string) {
                        Ok(_f) => {},
                        Err(_e) => {
                            println!("Error encountered opening file {}. Err: {}", destination_string, _e);
                            println!("Exiting."); return;
                        }
                    }
                },
            }
            write_results_to_file(fmt, &shared_files, &unique_files, &complete_files, destination_string);
        },
    }
}

fn write_results_to_file(fmt: PrintFmt, shared_files: &Vec<&Fileinfo>, unique_files: &Vec<&Fileinfo>, complete_files: &Vec<Fileinfo>, file: &str) {
    let mut output = fs::File::create(file).expect("Error opening output file for writing");
    match fmt {
        PrintFmt::Standard => {
            output.write_fmt(format_args!("Duplicates:\n")).unwrap();
            for file in shared_files.into_iter(){
                let title = file.file_paths.get(0).unwrap().file_name().unwrap().to_str().unwrap();
                output.write_fmt(format_args!("{}\n", title)).unwrap();
                for entry in file.file_paths.iter(){
                    output.write_fmt(format_args!("\t{}\n", entry.as_path().to_str().unwrap())).unwrap();
                }
            }
            output.write_fmt(format_args!("Singletons:\n")).unwrap();
            for file in unique_files.into_iter(){
                let title = file.file_paths.get(0).unwrap().file_name().unwrap().to_str().unwrap();
                output.write_fmt(format_args!("{}\n", title)).unwrap();
                for entry in file.file_paths.iter(){
                    output.write_fmt(format_args!("\t{}\n", entry.as_path().to_str().unwrap())).unwrap();
                }
            }
        },
        PrintFmt::Json => {
            output.write_fmt(format_args!("{}", serde_json::to_string(complete_files).unwrap_or("Error deserializing".to_string()))).unwrap();
        },
        PrintFmt::Off =>{return},
    }
    println!("{:#?} results written to {}", fmt, file);
}
