use std::{
    cmp::Reverse,
    fmt,
    fs::{read_dir, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
};

use clap::Parser;
use ordered_float::NotNan;
use rand::prelude::*;
use takzero::{
    network::{
        net5::{Env, Net, N},
        repr::{game_to_tensor, move_mask, output_size, policy_tensor},
        Network,
    },
    search::{agent::Agent, env::Environment, eval::Eval},
    target::{Augment, Target},
};
use tch::{
    nn::{Adam, Optimizer, OptimizerConfig},
    Device,
    Kind,
    Tensor,
};

// The environment to learn.
#[rustfmt::skip] #[allow(dead_code)]
const fn assert_env<E: Environment>() where Target<E>: Augment + fmt::Display {}
const _: () = assert_env::<Env>();

// The network architecture.
#[rustfmt::skip] #[allow(dead_code)] const fn assert_net<NET: Network + Agent<Env>>() {}
const _: () = assert_net::<Net>();

const DEVICE: Device = Device::Cuda(0);
const BATCH_SIZE: usize = 128;
const STEPS_PER_SAVE: usize = 10;
const STEPS_PER_CHECKPOINT: usize = 1000;
const LEARNING_RATE: f64 = 1e-4;

// Pre-training
const INITIAL_RANDOM_TARGETS: usize = BATCH_SIZE * 2_000;
const PRE_TRAINING_STEPS: usize = 1_000;
const _: () = assert!(INITIAL_RANDOM_TARGETS >= PRE_TRAINING_STEPS * BATCH_SIZE);

// Buffers
const STEPS_BEFORE_REANALYZE: usize = 5000;
const MIN_EXPLOITATION_BUFFER_LEN: usize = 2_000;
const _: () = assert!(MIN_EXPLOITATION_BUFFER_LEN >= BATCH_SIZE);
const MAX_EXPLOITATION_BUFFER_LEN: usize = 10_000;
const MAX_REANALYZE_BUFFER_LEN: usize = 10_000;
const EXPLOITATION_TARGET_USES_AVAILABLE: u32 = 1;
const REANALYZE_TARGET_USES_AVAILABLE: u32 = 1;

#[derive(Parser, Debug)]
struct Args {
    /// Directory where to find targets
    /// and also where to save models.
    #[arg(long)]
    directory: PathBuf,
}

struct TargetWithContext {
    /// The target.
    target: Target<Env>,
    /// How many uses are available until you cannot use this target.
    uses_available: u32,
    /// The model steps at the time of loading this target.
    model_steps: usize,
}

impl TargetWithContext {
    fn reuse(mut self) -> Option<Self> {
        if self.uses_available > 1 {
            self.uses_available -= 1;
            Some(self)
        } else {
            None
        }
    }
}

fn main() {
    env_logger::init();
    let args = Args::parse();

    let seed: u64 = rand::thread_rng().gen();
    log::info!("seed = {seed}");
    let mut rng = rand::rngs::StdRng::seed_from_u64(seed);

    // Load or initialize a network.
    let (mut net, mut starting_steps) = if let Some((starting_steps, model_path)) =
        get_model_path_with_most_steps(&args.directory)
    {
        log::info!(
            "Resuming at {starting_steps} steps with {}",
            model_path.display()
        );
        (
            Net::load(model_path, DEVICE).expect("Model file should be loadable"),
            starting_steps,
        )
    } else {
        log::info!("Creating new model");
        let net = Net::new(DEVICE, Some(rng.gen()));
        net.save(args.directory.join("model_000000.ot")).unwrap();
        (net, 0)
    };

    let mut opt = Adam::default().build(net.vs_mut(), LEARNING_RATE).unwrap();

    // Pre-training.
    if starting_steps == 0 {
        pre_training(&net, &mut opt, &mut rng, &args.directory);
        starting_steps += PRE_TRAINING_STEPS;
        net.save(
            args.directory
                .join(format!("model_{:0>6}.ot", starting_steps)),
        )
        .unwrap();
    }

    // Initialize buffers.
    let mut exploitation_buffer: Vec<TargetWithContext> =
        Vec::with_capacity(MAX_EXPLOITATION_BUFFER_LEN);
    let mut exploitation_targets_read = 0;
    let mut reanalyze_buffer: Vec<TargetWithContext> = Vec::with_capacity(MAX_REANALYZE_BUFFER_LEN);
    let mut reanalyze_targets_read = 0;

    // Main training loop.
    for model_steps in starting_steps.. {
        let using_reanalyze = model_steps >= STEPS_BEFORE_REANALYZE;
        fill_buffers(
            &mut exploitation_buffer,
            &mut exploitation_targets_read,
            &mut reanalyze_buffer,
            &mut reanalyze_targets_read,
            &args.directory,
            model_steps,
            using_reanalyze,
        );

        // Create a batch and take a step if there are enough targets.
        let enough_exploitation_targets = exploitation_buffer.len() >= MIN_EXPLOITATION_BUFFER_LEN;
        let enough_reanalyze_targets = !using_reanalyze || reanalyze_buffer.len() >= BATCH_SIZE / 2;
        if enough_exploitation_targets && enough_reanalyze_targets {
            let tensors = create_batch(
                using_reanalyze,
                &mut exploitation_buffer,
                &mut reanalyze_buffer,
                &mut rng,
            );
            compute_loss_and_take_step(&net, &mut opt, tensors);

            // Save latest model.
            if model_steps % STEPS_PER_SAVE == 0 {
                #[rustfmt::skip]
                log::info!(
                    "Saving model.\n\
                     Training steps: {model_steps}\n\
                     Exploitation buffer size: {}\n\
                     Reanalyze buffer size: {}",
                    exploitation_buffer.len(),
                    reanalyze_buffer.len()
                );

                let start = std::time::Instant::now();
                net.save(args.directory.join(format!("model_latest.ot")))
                    .unwrap();
                log::debug!("It took {:?} to save the latest model.", start.elapsed());
            }

            // Save checkpoint.
            if model_steps % STEPS_PER_CHECKPOINT == 0 {
                let start = std::time::Instant::now();
                net.save(args.directory.join(format!("model_{model_steps:0>6}.ot")))
                    .unwrap();
                log::debug!("It took {:?} to save the checkpoint.", start.elapsed());
                // I don't know if this helps or hurts or does nothing.
                opt.zero_grad();
            }
        } else {
            let duration = std::time::Duration::from_secs(30);
            #[rustfmt::skip]
            log::info!(
                "Not enough targets.\n\
                 Waiting {duration:?}.\n\
                 Training steps: {model_steps}\n\
                 Exploitation buffer size: {}\n\
                 Reanalyze buffer size: {}",
                exploitation_buffer.len(),
                reanalyze_buffer.len()
            );
            std::thread::sleep(duration);
        }
    }
}

/// Get the path to the model file (ending with ".ot")
/// which has the highest number of steps (number after '_')
/// in the given directory.
fn get_model_path_with_most_steps(directory: &PathBuf) -> Option<(usize, PathBuf)> {
    read_dir(directory)
        .unwrap()
        .filter_map(|res| res.ok().map(|entry| entry.path()))
        .filter(|p| p.extension().map(|ext| ext == "ot").unwrap_or_default())
        .filter_map(|p| {
            Some((
                p.file_stem()?
                    .to_str()?
                    .split_once('_')?
                    .1
                    .parse::<usize>()
                    .ok()?,
                p,
            ))
        })
        .max_by_key(|(s, _)| *s)
}

/// Add targets to the buffer from the given file, skipping the targets that
/// have already been read.
fn fill_buffer_with_targets(
    buffer: &mut Vec<TargetWithContext>,
    targets_already_read: &mut usize,
    file_path: &Path,
    uses_available: u32,
    model_steps: usize,
) -> std::io::Result<()> {
    buffer.extend(
        BufReader::new(OpenOptions::new().read(true).open(file_path)?)
            .lines()
            .skip(*targets_already_read)
            .map(|x| {
                *targets_already_read += 1;
                x.unwrap()
            })
            .filter_map(|line| line.parse().ok())
            .map(|target| TargetWithContext {
                target,
                uses_available,
                model_steps,
            }),
    );
    Ok(())
}

struct Tensors {
    input: Tensor,
    mask: Tensor,
    target_value: Tensor,
    target_policy: Tensor,
    #[allow(dead_code)]
    target_ube: Tensor,
}

fn create_input_and_target_tensors<'a>(
    batch: impl Iterator<Item = &'a Target<Env>>,
    rng: &mut impl Rng,
) -> Tensors {
    // Create input tensors.
    let mut inputs = Vec::with_capacity(BATCH_SIZE);
    let mut policy_targets = Vec::with_capacity(BATCH_SIZE);
    let mut masks = Vec::with_capacity(BATCH_SIZE);
    let mut value_targets = Vec::with_capacity(BATCH_SIZE);
    let mut ube_targets = Vec::with_capacity(BATCH_SIZE);
    for target in batch {
        let target = target.augment(rng);
        inputs.push(game_to_tensor(&target.env, DEVICE));
        policy_targets.push(policy_tensor::<N>(&target.policy, DEVICE));
        masks.push(move_mask::<N>(
            &target.policy.iter().map(|(m, _)| *m).collect::<Vec<_>>(),
            DEVICE,
        ));
        value_targets.push(target.value);
        ube_targets.push(target.ube);
    }

    // Get network output.
    let input = Tensor::cat(&inputs, 0).to(DEVICE);
    let mask = Tensor::cat(&masks, 0).to(DEVICE);
    // Get the target.
    let target_policy = Tensor::stack(&policy_targets, 0)
        .view([BATCH_SIZE as i64, output_size::<N>() as i64])
        .to(DEVICE);
    let target_value = Tensor::from_slice(&value_targets).unsqueeze(1).to(DEVICE);
    let target_ube = Tensor::from_slice(&ube_targets).unsqueeze(1).to(DEVICE);

    Tensors {
        input,
        mask,
        target_value,
        target_policy,
        target_ube,
    }
}

fn compute_loss_and_take_step(net: &Net, opt: &mut Optimizer, tensors: Tensors) {
    // Get network output.
    let (policy, network_value, _network_ube) = net.forward_t(&tensors.input, true);
    let log_softmax_network_policy = policy
        .masked_fill(&tensors.mask, f64::from(f32::MIN))
        .view([-1, output_size::<N>() as i64])
        .log_softmax(1, Kind::Float);

    // Calculate loss.
    let loss_policy = -(log_softmax_network_policy * &tensors.target_policy).sum(Kind::Float)
        / i64::try_from(BATCH_SIZE).unwrap();
    let loss_value = (tensors.target_value - network_value)
        .square()
        .mean(Kind::Float);
    // TODO: Add UBE back later.
    // let loss_ube = (target_ube - network_ube).square().mean(Kind::Float);
    let loss = &loss_policy + &loss_value; //+ &loss_ube;
    log::info!("loss = {loss:?}, loss_policy = {loss_policy:?}, loss_value = {loss_value:?}");

    // Take step.
    opt.backward_step(&loss);
}

fn pre_training(net: &Net, opt: &mut Optimizer, rng: &mut impl Rng, directory: &PathBuf) {
    let mut actions = Vec::new();
    let mut states = Vec::new();
    let mut buffer = Vec::with_capacity(INITIAL_RANDOM_TARGETS);
    while buffer.len() < INITIAL_RANDOM_TARGETS {
        let mut game = Env::new_opening(rng, &mut actions);
        // Play game until the end.
        while game.terminal().is_none() {
            states.push(game.clone());
            game.populate_actions(&mut actions);
            let action = actions.drain(..).choose(rng).unwrap();
            game.step(action);
        }
        // Create targets from the random game.
        let mut value = Eval::from(game.terminal().unwrap());
        for env in states.drain(..).rev() {
            env.populate_actions(&mut actions);
            // Uniform policy.
            let p = NotNan::new(1.0 / actions.len() as f32)
                .expect("there should always be at least one action");
            let policy = actions.drain(..).map(|a| (a, p)).collect();
            // Value is the discounted end of the game.
            value = value.negate();
            buffer.push(Target {
                env,
                policy,
                value: f32::from(value),
                ube: 1.0,
            });
        }
    }
    buffer.shuffle(rng);
    // Save initial targets for inspection.
    let content: String = buffer.iter().map(ToString::to_string).collect();
    OpenOptions::new()
        .write(true)
        .create(true)
        .open(directory.join("targets-initial.txt"))
        .unwrap()
        .write_all(content.as_bytes())
        .unwrap();

    for batch in buffer.chunks_exact(BATCH_SIZE).take(PRE_TRAINING_STEPS) {
        let tensors = create_input_and_target_tensors(batch.into_iter(), rng);
        compute_loss_and_take_step(net, opt, tensors);
    }
}

fn create_batch(
    using_reanalyze: bool,
    exploitation_buffer: &mut Vec<TargetWithContext>,
    reanalyze_buffer: &mut Vec<TargetWithContext>,
    rng: &mut impl Rng,
) -> Tensors {
    // TODO: Can we avoid doing an O(n) operation here?
    // Ideally we would like to sample without replacement,
    // Then swap_remove those targets which have uses_available == 0.
    exploitation_buffer.shuffle(rng);
    reanalyze_buffer.shuffle(rng);

    if using_reanalyze {
        let batch: Vec<_> = exploitation_buffer
            .drain(exploitation_buffer.len() - BATCH_SIZE / 2..)
            .chain(reanalyze_buffer.drain(reanalyze_buffer.len() - BATCH_SIZE / 2..))
            .collect();
        let tensors = create_input_and_target_tensors(batch.iter().map(|t| &t.target), rng);
        let mut iter = batch.into_iter();
        exploitation_buffer.extend(
            iter.by_ref()
                .take(BATCH_SIZE / 2)
                .filter_map(TargetWithContext::reuse),
        );
        reanalyze_buffer.extend(iter.filter_map(TargetWithContext::reuse));
        return tensors;
    }

    let batch: Vec<_> = exploitation_buffer
        .drain(exploitation_buffer.len() - BATCH_SIZE..)
        .collect();
    let tensors = create_input_and_target_tensors(batch.iter().map(|t| &t.target), rng);
    exploitation_buffer.extend(batch.into_iter().filter_map(TargetWithContext::reuse));
    tensors
}

fn truncate_buffer_if_needed(buffer: &mut Vec<TargetWithContext>, max_length: usize, name: &str) {
    if buffer.len() > max_length {
        log::info!(
            "Truncating {name} buffer because it is too big. {}",
            buffer.len()
        );
        buffer.sort_unstable_by_key(|t| Reverse((t.model_steps, t.uses_available)));
        buffer.truncate(max_length);
    }
}

fn fill_buffers(
    exploitation_buffer: &mut Vec<TargetWithContext>,
    exploitation_targets_read: &mut usize,
    reanalyze_buffer: &mut Vec<TargetWithContext>,
    reanalyze_targets_read: &mut usize,
    directory: &Path,
    model_steps: usize,
    using_reanalyze: bool,
) {
    let start = std::time::Instant::now();

    if let Err(error) = fill_buffer_with_targets(
        exploitation_buffer,
        exploitation_targets_read,
        &directory.join("targets-selfplay.txt"),
        EXPLOITATION_TARGET_USES_AVAILABLE,
        model_steps,
    ) {
        log::error!("Cannot read selfplay targets: {error}");
    }
    truncate_buffer_if_needed(
        exploitation_buffer,
        MAX_EXPLOITATION_BUFFER_LEN,
        "exploitation",
    );
    if using_reanalyze {
        if let Err(error) = fill_buffer_with_targets(
            reanalyze_buffer,
            reanalyze_targets_read,
            &directory.join("targets-reanalyze.txt"),
            REANALYZE_TARGET_USES_AVAILABLE,
            model_steps,
        ) {
            log::error!("Cannot read reanalyze targets: {error}");
        }
        truncate_buffer_if_needed(reanalyze_buffer, MAX_EXPLOITATION_BUFFER_LEN, "reanalyze");
    }

    log::debug!("It took {:?} to add targets to buffer.", start.elapsed());
}
