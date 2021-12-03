use anyhow::Result;
use bio::stats::bayesian::model::Model;
use bio::stats::probs::{LogProb, Prob};
use bio_types::genome::AbstractLocus;
use derive_builder::Builder;
use hdf5;
use ordered_float::NotNaN;
use rust_htslib::bcf;
use rust_htslib::bcf::record::GenotypeAllele::Unphased;
use rust_htslib::bcf::Read;
use std::collections::HashMap;
use std::convert::TryFrom;
use std::io;
use std::io::Read as OtherRead;
use std::path::PathBuf;

use crate::haplotypes::model::{Data, HaplotypeFractions, Likelihood, Marginal, Posterior, Prior};

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Caller {
    hdf5_reader: hdf5::File,
    haplotype_variants: bcf::Reader,
    haplotype_calls: bcf::Reader,
    min_norm_counts: f64,
    outcsv: Option<PathBuf>,
}

impl Caller {
    pub fn call(&mut self) -> Result<()> {
        // Step 1: obtain kallisto estimates.
        let kallisto_estimates = KallistoEstimates::new(&self.hdf5_reader, self.min_norm_counts)?;
        let haplotype_variants = HaplotypeVariants::new(&mut self.haplotype_variants)?;

        // Step 2: setup model.
        let model = Model::new(Likelihood::new(), Prior::new(), Posterior::new());

        //let universe = HaplotypeFractions::likely(&kallisto_estimates);
        let data = Data::new(kallisto_estimates.values().cloned().collect());

        // Step 3: calculate posteriors.
        //let m = model.compute(universe, &data);
        let haplotypes: Vec<_> = kallisto_estimates.keys().map(|x| x.to_string()).collect();
        let m = model.compute_from_marginal(&Marginal::new(haplotypes.len()), &data);

        // Step 4: print TSV table with results
        // TODO use csv crate
        // Columns: posterior_prob, haplotype_a, haplotype_b, haplotype_c, ...
        // with each column after the first showing the fraction of the respective haplotype
        let mut posterior = m.event_posteriors();
        let mut wtr = csv::Writer::from_path(self.outcsv.as_ref().unwrap())?;
        let mut headers: Vec<_> = vec!["density".to_string(), "odds".to_string()];
        headers.extend(haplotypes);
        wtr.write_record(&headers)?;

        //write best record on top
        let mut records = Vec::new();
        let best = posterior.next().unwrap();
        let (haplotype_frequencies, best_density) = posterior.next().unwrap();
        let best_odds = 1;
        records.push(best_density.exp().to_string());
        records.push(best_odds.to_string());

        for frequency in haplotype_frequencies.iter() {
            records.push(frequency.to_string());
        }
        wtr.write_record(records)?;

        //write the rest of the records
        for (haplotype_frequencies, density) in posterior {
            let mut records = Vec::new();
            let odds = (density - best_density).exp();
            records.push(density.exp().to_string());
            records.push(odds.to_string());
            for frequency in haplotype_frequencies.iter() {
                records.push(frequency.to_string());
            }
            wtr.write_record(records)?;
        }
        Ok(())
    }
}

#[derive(Derefable, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Haplotype(#[deref] String);

#[derive(Debug, Clone)]
pub(crate) struct KallistoEstimate {
    pub count: f64,
    pub dispersion: f64,
}

#[derive(Debug, Clone, Derefable)]
pub(crate) struct KallistoEstimates(#[deref] HashMap<Haplotype, KallistoEstimate>);

impl KallistoEstimates {
    /// Generate new instance.
    pub(crate) fn new(hdf5_reader: &hdf5::File, min_norm_counts: f64) -> Result<Self> {
        let seqnames = Self::filter_seqnames(hdf5_reader, min_norm_counts)?;

        let ids = hdf5_reader
            .dataset("aux/ids")?
            .read_1d::<hdf5::types::FixedAscii<255>>()?;
        let num_bootstraps = hdf5_reader.dataset("aux/num_bootstrap")?.read_1d::<i32>()?;
        let seq_length = hdf5_reader.dataset("aux/lengths")?.read_1d::<f64>()?;

        let mut estimates = HashMap::new();

        for seqname in seqnames {
            let index = ids.iter().position(|x| x.as_str() == seqname).unwrap();
            let mut bootstraps = Vec::new();
            for i in 0..num_bootstraps[0] {
                let dataset = hdf5_reader.dataset(&format!("bootstrap/bs{i}", i = i))?;
                let est_counts = dataset.read_1d::<f64>()?;
                let norm_counts = est_counts / &seq_length;
                let norm_counts = norm_counts[index];
                bootstraps.push(norm_counts);
            }
            //mean
            let sum = bootstraps.iter().sum::<f64>();
            let count = bootstraps.len();
            let m = sum / count as f64;

            //std dev
            let variance = bootstraps
                .iter()
                .map(|value| {
                    let diff = m - (*value as f64);
                    diff * diff
                })
                .sum::<f64>()
                / count as f64;
            let std = variance.sqrt();
            let t = std / m;

            //retrieval of mle
            let mle_dataset = hdf5_reader.dataset("est_counts")?.read_1d::<f64>()?;
            let mle_norm = mle_dataset / &seq_length; //normalized mle counts by length
            let m = mle_norm[index];

            estimates.insert(
                Haplotype(seqname.clone()),
                KallistoEstimate {
                    dispersion: t,
                    count: m,
                },
            );
        }

        Ok(KallistoEstimates(estimates))
    }

    //Return a vector of filtered seqnames according to --min-norm-counts.
    fn filter_seqnames(hdf5_reader: &hdf5::File, min_norm_counts: f64) -> Result<Vec<String>> {
        let est_counts = hdf5_reader.dataset("est_counts")?.read_1d::<f64>()?;
        let seq_length = hdf5_reader.dataset("aux/lengths")?.read_1d::<f64>()?; //these two variables arrays have the same length.
        let norm_counts = est_counts / seq_length;
        let mut indices = Vec::new();
        for (i, num) in norm_counts.iter().enumerate() {
            if num > &min_norm_counts {
                indices.push(i);
            }
        }
        let ids = hdf5_reader
            .dataset("aux/ids")?
            .read_1d::<hdf5::types::FixedAscii<255>>()?;

        let mut filtered: Vec<String> = Vec::new();
        for i in indices {
            filtered.push(ids[i].to_string());
        }
        Ok(filtered)
    }
}

#[derive(Derefable, Debug, Clone, PartialEq, Eq, Hash, new)]
pub(crate) struct Variant(#[deref] String);

#[derive(Derefable, Debug, Clone, PartialEq, Eq)]
pub(crate) struct HaplotypeVariants(#[deref] HashMap<Variant, Vec<Haplotype>>);

impl HaplotypeVariants {
    pub(crate) fn new(haplotype_variants: &mut bcf::Reader) -> Result<Vec<HaplotypeVariants>> {
        let mut vec_haplotype_variants = Vec::new();
        for record_result in haplotype_variants.records() {
            let record = record_result.expect("Failed to read record");

            //store the variant
            let mut variant = String::new();
            variant.push_str(&format!("{}{}", record.contig(), "_"));
            let pos = record.pos().to_string();
            variant.push_str(&format!("{}{}", pos, "_"));
            for allele in record.alleles() {
                for c in allele {
                    let base = char::from(*c).to_string();
                    variant.push_str(&format!("{}{}", base, "_"));
                }
            }

            //store the haplotypes that carry the variant
            let header = record.header();
            let gts = record.genotypes().expect("Error reading genotypes"); //genotypes of all samples
            let sample_count = usize::try_from(record.sample_count()).unwrap();
            let sample = (0..sample_count).collect::<Vec<_>>();
            let mut haplotypes: Vec<Haplotype> = Vec::new();

            for (sample_index, mut x) in sample.iter().zip(header.samples().into_iter()) {
                for gta in gts.get(*sample_index).iter() {
                    if gta == &Unphased(1) {
                        let mut s = String::new();
                        x.read_to_string(&mut s);
                        haplotypes.push(Haplotype(s));
                    } else {
                        break;
                    }
                }
            }
            let mut haplotype_variants = HashMap::new();
            haplotype_variants.insert(Variant(variant.clone()), haplotypes);
            let haplotype_variants = HaplotypeVariants(haplotype_variants);
            vec_haplotype_variants.push(haplotype_variants);
        }
        Ok(vec_haplotype_variants)
    }
}